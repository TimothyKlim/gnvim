use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gdk;
use glib;
use gtk;
use neovim_lib::neovim::Neovim;
use neovim_lib::neovim_api::NeovimApi;
use neovim_lib::NeovimApiAsync;
use neovim_lib::Value;

use gtk::prelude::*;

use nvim_bridge::{
    CmdlinePos, CmdlineSpecialChar, DefaultColorsSet, GnvimEvent,
    GridCursorGoto, GridResize, HlAttrDefine, Message, ModeChange, ModeInfo,
    ModeInfoSet, Notify, OptionSet, RedrawEvent, Request, TablineUpdate,
};
use ui::cmdline::Cmdline;
use ui::color::{Color, Highlight};
use ui::cursor_tooltip::{CursorTooltip, Gravity};
use ui::font::Font;
use ui::grid::Grid;
use ui::popupmenu::Popupmenu;
use ui::tabline::Tabline;
use ui::window::Window;

type Windows = HashMap<i64, Window>;
type Grids = HashMap<i64, Grid>;

#[derive(Default)]
pub struct HlDefs {
    hl_defs: HashMap<u64, Highlight>,

    pub default_fg: Color,
    pub default_bg: Color,
    pub default_sp: Color,
}

impl HlDefs {
    pub fn get_mut(&mut self, id: &u64) -> Option<&mut Highlight> {
        self.hl_defs.get_mut(id)
    }

    pub fn get(&self, id: &u64) -> Option<&Highlight> {
        self.hl_defs.get(id)
    }

    pub fn insert(&mut self, id: u64, hl: Highlight) -> Option<Highlight> {
        self.hl_defs.insert(id, hl)
    }
}

struct ResizeOptions {
    font: Font,
    line_space: i64,
}

/// Internal structure for `UI` to work on.
struct UIState {
    css_provider: gtk::CssProvider,
    windows: Windows,
    /// Container for non-floating windows.
    windows_container: gtk::Fixed,
    msg_grid_container: gtk::Fixed,
    msg_grid_frame: gtk::Frame,
    /// Container for floating windows.
    windows_float_container: gtk::Fixed,
    /// All grids currently in the UI.
    grids: Grids,
    /// Highlight definitions.
    hl_defs: HlDefs,
    /// Mode infos. When a mode is activated, the activated mode is passed
    /// to the gird(s).
    mode_infos: Vec<ModeInfo>,
    /// Current mode.
    current_mode: Option<ModeInfo>,
    /// Id of the current active grid.
    current_grid: i64,

    popupmenu: Popupmenu,
    cmdline: Cmdline,
    tabline: Tabline,
    cursor_tooltip: CursorTooltip,

    /// Overlay contains our grid(s) and popupmenu.
    #[allow(unused)]
    overlay: gtk::Overlay,

    /// Source id for delayed call to ui_try_resize.
    resize_source_id: Rc<RefCell<Option<glib::SourceId>>>,
    /// Resize options that is some if a resize should be send to nvim on flush.
    resize_on_flush: Option<ResizeOptions>,

    font: Font,
    line_space: i64,
}

/// Main UI structure.
pub struct UI {
    /// Main window.
    win: gtk::ApplicationWindow,
    /// Neovim instance.
    nvim: Rc<RefCell<Neovim>>,
    /// Channel to receive event from nvim.
    rx: glib::Receiver<Message>,
    /// Our internal state, containing basically everything we manipulate
    /// when we receive an event from nvim.
    state: Rc<RefCell<UIState>>,
}

impl UI {
    /// Creates new UI.
    ///
    /// * `app` - GTK application for the UI.
    /// * `rx` - Channel to receive nvim UI events.
    /// * `nvim` - Neovim instance to use. Should be the same that is the source
    ///            of `rx` events.
    pub fn init(
        app: &gtk::Application,
        rx: glib::Receiver<Message>,
        nvim: Rc<RefCell<Neovim>>,
    ) -> Self {
        // Create the main window.
        let window = gtk::ApplicationWindow::new(app);
        window.set_title("Neovim");
        window.set_default_size(1280, 720);

        // Realize window resources.
        window.realize();

        // Top level widget.
        let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
        window.add(&b);

        let tabline = Tabline::new(nvim.clone());
        b.pack_start(&tabline.get_widget(), false, false, 0);

        // Our root widget for all grids/windows.
        let overlay = gtk::Overlay::new();
        b.pack_start(&overlay, true, true, 0);

        // Create hl defs and initialize 0th element because we'll need to have
        // something that is accessible for the default grid that we're gonna
        // make next.
        let mut hl_defs = HlDefs::default();
        hl_defs.insert(0, Highlight::default());

        let font = Font::from_guifont("Monospace:h12").unwrap();
        let line_space = 0;

        // Create default grid.
        let mut grid = Grid::new(
            1,
            &window.get_window().unwrap(),
            font.clone(),
            line_space,
            80,
            30,
        );
        overlay.add(&grid.widget());

        let windows_container = gtk::Fixed::new();
        gtk::WidgetExt::set_name(&windows_container, "Windows contianer");
        let windows_float_container = gtk::Fixed::new();
        gtk::WidgetExt::set_name(
            &windows_float_container,
            "Floating windows contianer",
        );
        let msg_grid_container = gtk::Fixed::new();
        gtk::WidgetExt::set_name(
            &msg_grid_container,
            "Message grid contianer",
        );
        overlay.add_overlay(&windows_container);
        overlay.add_overlay(&windows_float_container);

        let msg_grid_frame = gtk::Frame::new(None);
        msg_grid_container.put(&msg_grid_frame, 0, 0);
        overlay.add_overlay(&msg_grid_container);

        // TODO(ville): is pass through for windows_container required in any case?
        //overlay.set_overlay_pass_through(&windows_container, true);
        overlay.set_overlay_pass_through(&windows_float_container, true);

        // When resizing our window (main grid), we'll have to tell neovim to
        // resize it self also. The notify to nvim is send with a small delay,
        // so we don't spam it multiple times a second. source_id is used to
        // track the function timeout. This timeout might be canceled in
        // redraw even handler if we receive a message that changes the size
        // of the main grid.
        let source_id = Rc::new(RefCell::new(None));
        grid.connect_da_resize(clone!(nvim, source_id => move |rows, cols| {

            // Set timeout to notify nvim about the new size.
            let new = gtk::timeout_add(30, clone!(nvim, source_id => move || {
                let mut nvim = nvim.borrow_mut();
                nvim.ui_try_resize_async(cols as i64, rows as i64)
                    .cb(|res| {
                        if let Err(err) = res {
                            eprintln!("Error: failed to resize nvim when grid size changed ({:?})", err);
                        }
                    })
                .call();

                // Set the source_id to none, so we don't accidentally remove
                // it since it used at this point.
                source_id.borrow_mut().take();

                Continue(false)
            }));

            let mut source_id = source_id.borrow_mut();
            // If we have earlier timeout, remove it.
            if let Some(old) = source_id.take() {
                glib::source::source_remove(old);
            }

            *source_id = Some(new);

            false
        }));

        attach_grid_events(&grid, nvim.clone());

        // IMMulticontext is used to handle most of the inputs.
        let im_context = gtk::IMMulticontext::new();
        im_context.set_use_preedit(false);
        im_context.connect_commit(clone!(nvim => move |_, input| {
            // "<" needs to be escaped for nvim.input()
            let nvim_input = input.replace("<", "<lt>");

            let mut nvim = nvim.borrow_mut();
            nvim.input(&nvim_input).expect("Couldn't send input");
        }));

        window.connect_key_press_event(clone!(nvim, im_context => move |_, e| {
            if im_context.filter_keypress(e) {
                Inhibit(true)
            } else {
                if let Some(input) = event_to_nvim_input(e) {
                    let mut nvim = nvim.borrow_mut();
                    nvim.input(input.as_str()).expect("Couldn't send input");
                    return Inhibit(true);
                } else {
                    println!(
                        "Failed to turn input event into nvim key (keyval: {})",
                        e.get_keyval()
                    )
                }

                Inhibit(false)
            }
        }));

        window.connect_key_release_event(clone!(im_context => move |_, e| {
            im_context.filter_keypress(e);
            Inhibit(false)
        }));

        window.connect_focus_in_event(clone!(im_context => move |_, _| {
            im_context.focus_in();
            Inhibit(false)
        }));

        window.connect_focus_out_event(clone!(im_context => move |_, _| {
            im_context.focus_out();
            Inhibit(false)
        }));

        let cmdline = Cmdline::new(&overlay, nvim.clone());
        let cursor_tooltip = CursorTooltip::new(&overlay);

        window.show_all();

        grid.set_im_context(&im_context);

        cmdline.hide();
        cursor_tooltip.hide();

        let mut grids = HashMap::new();
        grids.insert(1, grid);

        let css_provider = gtk::CssProvider::new();
        add_css_provider!(&css_provider, window);

        UI {
            win: window,
            rx,
            state: Rc::new(RefCell::new(UIState {
                css_provider,
                windows: Windows::new(),
                windows_container,
                msg_grid_container,
                msg_grid_frame,
                windows_float_container,
                grids,
                mode_infos: vec![],
                current_grid: 1,
                popupmenu: Popupmenu::new(&overlay, nvim.clone()),
                cmdline,
                overlay,
                tabline,
                cursor_tooltip,
                resize_source_id: source_id,
                hl_defs,
                resize_on_flush: None,
                font,
                line_space,
                current_mode: None,
            })),
            nvim,
        }
    }

    /// Starts to listen events from `rx` (e.g. from nvim) and processing those.
    /// Think this as the "main" function of the UI.
    pub fn start(self) {
        let UI {
            rx,
            state,
            win,
            nvim,
        } = self;

        gtk::timeout_add(
            33,
            clone!(state => move || {
                let state = state.borrow();
                // Tick the current active grid.
                let grid =
                    state.grids.get(&state.current_grid).unwrap();
                grid.tick();

                glib::Continue(true)
            }),
        );

        rx.attach(None, move |message| {
            match message {
                // Handle a notify.
                Message::Notify(notify) => {
                    let mut state = state.borrow_mut();

                    handle_notify(&win, notify, &mut state, nvim.clone());
                }
                // Handle a request.
                Message::Request(tx, request) => {
                    let mut state = state.borrow_mut();
                    let res = handle_request(&request, &mut state);
                    tx.send(res).expect("Failed to respond to a request");
                }
                // Handle close.
                Message::Close => {
                    win.close();
                    return Continue(false);
                }
            }

            Continue(true)
        });
    }
}

fn handle_request(
    request: &Request,
    state: &mut UIState,
) -> Result<Value, Value> {
    match request {
        Request::CursorTooltipStyles => {
            let styles = state.cursor_tooltip.get_styles();

            let res: Vec<Value> =
                styles.into_iter().map(|s| s.into()).collect();

            Ok(res.into())
        }
    }
}

fn handle_notify(
    window: &gtk::ApplicationWindow,
    notify: Notify,
    state: &mut UIState,
    nvim: Rc<RefCell<Neovim>>,
) {
    match notify {
        Notify::RedrawEvent(events) => {
            handle_redraw_event(window, events, state, nvim);
        }
        Notify::GnvimEvent(event) => match event {
            Ok(event) => handle_gnvim_event(event, state, nvim),
            Err(err) => {
                let mut nvim = nvim.borrow_mut();
                nvim.command_async(&format!(
                    "echom \"Failed to parse gnvim notify: '{}'\"",
                    err
                ))
                .cb(|res| match res {
                    Ok(_) => {}
                    Err(err) => {
                        println!("Failed to execute nvim command: {}", err)
                    }
                })
                .call();
            }
        },
    }
}

fn handle_gnvim_event(
    event: GnvimEvent,
    state: &mut UIState,
    nvim: Rc<RefCell<Neovim>>,
) {
    match event {
        GnvimEvent::SetGuiColors(colors) => {
            state.popupmenu.set_colors(colors.pmenu, &state.hl_defs);
            state.tabline.set_colors(colors.tabline, &state.hl_defs);
            state.cmdline.set_colors(colors.cmdline, &state.hl_defs);
            state
                .cmdline
                .wildmenu_set_colors(&colors.wildmenu, &state.hl_defs);
        }
        GnvimEvent::CompletionMenuToggleInfo => {
            state.popupmenu.toggle_show_info()
        }
        GnvimEvent::CursorTooltipLoadStyle(path) => {
            if let Err(err) = state.cursor_tooltip.load_style(path.clone()) {
                let mut nvim = nvim.borrow_mut();
                nvim.command_async(&format!(
                    "echom \"Cursor tooltip load style failed: '{}'\"",
                    err
                ))
                .cb(|res| match res {
                    Ok(_) => {}
                    Err(err) => {
                        println!("Failed to execute nvim command: {}", err)
                    }
                })
                .call();
            }
        }
        GnvimEvent::CursorTooltipShow(content, row, col) => {
            state.cursor_tooltip.show(content.clone());

            let grid = state.grids.get(&state.current_grid).unwrap();
            let mut rect = grid.get_rect_for_cell(row, col);

            let window = state.windows.get(&state.current_grid).unwrap();
            rect.x += window.x as i32;
            rect.y += window.y as i32;

            state.cursor_tooltip.move_to(&rect);
        }
        GnvimEvent::CursorTooltipHide => state.cursor_tooltip.hide(),
        GnvimEvent::CursorTooltipSetStyle(style) => {
            state.cursor_tooltip.set_style(&style)
        }
        GnvimEvent::PopupmenuWidth(width) => {
            state.popupmenu.set_width(width as i32);
        }
        GnvimEvent::PopupmenuWidthDetails(width) => {
            state.popupmenu.set_width_details(width as i32);
        }
        GnvimEvent::Unknown(msg) => {
            println!("Received unknown GnvimEvent: {}", msg);
        }
    }
}

fn handle_redraw_event(
    window: &gtk::ApplicationWindow,
    events: Vec<RedrawEvent>,
    state: &mut UIState,
    nvim: Rc<RefCell<Neovim>>,
) {
    for event in events.into_iter() {
        match event {
            RedrawEvent::SetTitle(evt) => {
                evt.iter().for_each(|title| {
                    window.set_title(title);
                });
            }
            RedrawEvent::GridLine(evt) => {
                evt.iter().for_each(|line| {
                    let grid = state.grids.get(&line.grid).unwrap();
                    grid.put_line(line, &state.hl_defs);
                });
            }
            RedrawEvent::GridCursorGoto(evt) => {
                evt.iter().for_each(
                    |GridCursorGoto {
                         grid: grid_id,
                         row,
                         col,
                     }| {
                        // Gird cursor goto sets the current cursor to grid_id,
                        // so we'll need to handle that here...
                        let grid = if *grid_id != state.current_grid {
                            // ...so if the grid_id is not same as the state tells us,
                            // set the previous current grid to inactive state.
                            let grid =
                                state.grids.get(&state.current_grid).unwrap();

                            grid.set_active(false);
                            grid.tick(); // Trick the grid to invalide the cursor's rect.

                            state.current_grid = *grid_id;

                            // And set the new current grid to active.
                            let grid = state.grids.get(grid_id).unwrap();
                            grid.set_active(true);
                            grid
                        } else {
                            state.grids.get(grid_id).unwrap()
                        };

                        // And after all that, set the current grid's cursor position.
                        grid.cursor_goto(*row, *col);
                    },
                );
            }
            RedrawEvent::GridResize(evt) => {
                evt.iter().for_each(
                    |GridResize {
                         grid: id,
                         width,
                         height,
                     }| {
                        let win = window.get_window().unwrap();
                        if let Some(grid) = state.grids.get(id) {
                            grid.resize(&win, *width, *height);
                        } else {
                            let grid = Grid::new(
                                *id,
                                &window.get_window().unwrap(),
                                state.font.clone(),
                                state.line_space,
                                *width as usize,
                                *height as usize,
                            );

                            if let Some(ref mode) = state.current_mode {
                                grid.set_mode(&mode);
                            }
                            grid.resize(&win, *width, *height);
                            attach_grid_events(&grid, nvim.clone());
                            state.grids.insert(*id, grid);
                        }
                    },
                );
            }
            RedrawEvent::GridClear(evt) => {
                evt.iter().for_each(|grid| {
                    let grid = state.grids.get(grid).unwrap();
                    grid.clear(&state.hl_defs);
                });
            }
            RedrawEvent::GridDestroy(evt) => {
                evt.iter().for_each(|grid| {
                    println!("DROP GRID {}", grid);
                    // TODO(ville): Make sure all grid resources are freed.
                    state.grids.remove(grid).unwrap(); // Drop grid.

                    // Make the current grid to point to the default grid. We relay on the fact
                    // that current_grid is always pointing to a existing grid.
                    state.current_grid = 1;
                });
            }
            RedrawEvent::GridScroll(evt) => {
                evt.iter().for_each(|info| {
                    let grid = state.grids.get(&info.grid).unwrap();
                    grid.scroll(info.reg, info.rows, info.cols, &state.hl_defs);

                    let mut nvim = nvim.borrow_mut();
                    // Since nvim doesn't have its own 'scroll' autocmd, we'll
                    // have to do it on our own. This use useful for the cursor tooltip.
                    nvim.command_async("if exists('#User#GnvimScroll') | doautocmd User GnvimScroll | endif")
                     .cb(|res| match res {
                         Ok(_) => {}
                         Err(err) => println!("GnvimScroll error: {:?}", err),
                     })
                     .call();
                });
            }
            RedrawEvent::DefaultColorsSet(evt) => {
                evt.iter().for_each(|DefaultColorsSet { fg, bg, sp }| {
                    state.hl_defs.default_fg = *fg;
                    state.hl_defs.default_bg = *bg;
                    state.hl_defs.default_sp = *sp;

                    {
                        // NOTE(ville): Not sure if these are actually needed.
                        let hl = state.hl_defs.get_mut(&0).unwrap();
                        hl.foreground = Some(*fg);
                        hl.background = Some(*bg);
                        hl.special = Some(*sp);
                    }

                    for grid in state.grids.values() {
                        grid.redraw(&state.hl_defs);
                    }

                    state.cursor_tooltip.set_colors(*fg, *bg);

                    // Set the background for our main window.
                    CssProviderExt::load_from_data(
                        &state.css_provider,
                        format!(
                            "* {{
                            background: #{bg};
                        }}",
                            bg = bg.to_hex()
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                });
            }
            RedrawEvent::HlAttrDefine(evt) => {
                evt.iter().for_each(|HlAttrDefine { id, hl }| {
                    state.hl_defs.insert(*id, *hl);
                });
            }
            RedrawEvent::OptionSet(evt) => {
                evt.iter().for_each(|opt| match opt {
                    OptionSet::GuiFont(font) => {
                        let font =
                            Font::from_guifont(font).unwrap_or(Font::default());

                        state.font = font.clone();

                        let mut opts =
                            state.resize_on_flush.take().unwrap_or_else(|| {
                                let grid = state.grids.get(&1).unwrap();
                                ResizeOptions {
                                    font: grid.get_font(),
                                    line_space: grid.get_line_space(),
                                }
                            });

                        opts.font = font;

                        state.resize_on_flush = Some(opts);
                    }
                    OptionSet::LineSpace(val) => {
                        state.line_space = *val;

                        let mut opts =
                            state.resize_on_flush.take().unwrap_or_else(|| {
                                let grid = state.grids.get(&1).unwrap();
                                ResizeOptions {
                                    font: grid.get_font(),
                                    line_space: grid.get_line_space(),
                                }
                            });

                        opts.line_space = *val;

                        state.resize_on_flush = Some(opts);
                    }
                    OptionSet::NotSupported(name) => {
                        println!("Not supported option set: {}", name);
                    }
                });
            }
            RedrawEvent::ModeInfoSet(evt) => {
                evt.iter().for_each(|ModeInfoSet { mode_info, .. }| {
                    state.mode_infos = mode_info.clone();
                });
            }
            RedrawEvent::ModeChange(evt) => {
                evt.iter().for_each(|ModeChange { index, .. }| {
                    let mode = state.mode_infos.get(*index as usize).unwrap();
                    state.current_mode = Some(mode.clone());
                    // Broadcast the mode change to all grids.
                    // TODO(ville): It might be enough to just set the mode to the
                    //              current active grid.
                    for grid in state.grids.values() {
                        grid.set_mode(mode);
                    }
                });
            }
            RedrawEvent::SetBusy(busy) => {
                for grid in state.grids.values() {
                    grid.set_busy(busy);
                }
            }
            RedrawEvent::Flush() => {
                for grid in state.grids.values() {
                    grid.flush(&state.hl_defs);
                }

                if let Some(opts) = state.resize_on_flush.take() {
                    let win = window.get_window().unwrap();
                    for grid in state.grids.values() {
                        grid.update_cell_metrics(
                            opts.font.clone(),
                            opts.line_space,
                            &win,
                        );
                    }

                    // TODO(ville): Use the root container to get the main grid's size and make
                    // sure that it (the root contianer widget) has proper size.
                    let grid = state.grids.get(&1).unwrap();
                    let (cols, rows) = grid.calc_size();

                    // Cancel any possible delayed call for ui_try_resize.
                    let mut id = state.resize_source_id.borrow_mut();
                    if let Some(id) = id.take() {
                        glib::source::source_remove(id);
                    }

                    nvim.borrow_mut().ui_try_resize_async(cols as i64, rows as i64)
                        .cb(|res| {
                            if let Err(err) = res {
                                eprintln!("Error: failed to resize nvim on line space change ({:?})", err);
                            }
                        })
                    .call();

                    state.popupmenu.set_font(opts.font.clone(), &state.hl_defs);
                    state.cmdline.set_font(opts.font.clone(), &state.hl_defs);
                    state.tabline.set_font(opts.font.clone(), &state.hl_defs);
                    state.cursor_tooltip.set_font(opts.font.clone());

                    state.cmdline.set_line_space(opts.line_space);
                    state
                        .popupmenu
                        .set_line_space(opts.line_space, &state.hl_defs);
                    state
                        .tabline
                        .set_line_space(opts.line_space, &state.hl_defs);
                }
            }
            RedrawEvent::PopupmenuShow(evt) => {
                evt.iter().for_each(|popupmenu| {
                    state
                        .popupmenu
                        .set_items(popupmenu.items.clone(), &state.hl_defs);

                    let grid = state.grids.get(&state.current_grid).unwrap();
                    let mut rect =
                        grid.get_rect_for_cell(popupmenu.row, popupmenu.col);

                    let window = state.windows.get(&popupmenu.grid).unwrap();
                    rect.x += window.x as i32;
                    rect.y += window.y as i32;

                    state.popupmenu.set_anchor(rect);
                    state.popupmenu.show();
                    state
                        .popupmenu
                        .select(popupmenu.selected as i32, &state.hl_defs);

                    // If the cursor tooltip is visible at the same time, move
                    // it out of our way.
                    if state.cursor_tooltip.is_visible() {
                        if state.popupmenu.is_above_anchor() {
                            state
                                .cursor_tooltip
                                .force_gravity(Some(Gravity::Down));
                        } else {
                            state
                                .cursor_tooltip
                                .force_gravity(Some(Gravity::Up));
                        }

                        state.cursor_tooltip.refresh_position();
                    }
                });
            }
            RedrawEvent::PopupmenuHide() => {
                state.popupmenu.hide();

                // Undo any force positioning of cursor tool tip that might
                // have occured on popupmenu show.
                state.cursor_tooltip.force_gravity(None);
                state.cursor_tooltip.refresh_position();
            }
            RedrawEvent::PopupmenuSelect(evt) => {
                evt.iter().for_each(|selected| {
                    state.popupmenu.select(*selected as i32, &state.hl_defs);
                });
            }
            RedrawEvent::TablineUpdate(evt) => {
                evt.iter().for_each(|TablineUpdate { current, tabs }| {
                    let mut nvim = nvim.borrow_mut();
                    state.tabline.update(
                        &mut nvim,
                        current.clone(),
                        tabs.clone(),
                    );
                });
            }
            RedrawEvent::CmdlineShow(evt) => {
                evt.iter().for_each(|cmdline_show| {
                    state.cmdline.show(cmdline_show, &state.hl_defs);
                });
            }
            RedrawEvent::CmdlineHide() => {
                state.cmdline.hide();
            }
            RedrawEvent::CmdlinePos(evt) => {
                evt.iter().for_each(|CmdlinePos { pos, level }| {
                    state.cmdline.set_pos(*pos, *level);
                });
            }
            RedrawEvent::CmdlineSpecialChar(evt) => {
                evt.iter().for_each(
                    |CmdlineSpecialChar {
                         character: ch,
                         shift,
                         level,
                     }| {
                        state.cmdline.show_special_char(
                            ch.clone(),
                            *shift,
                            *level,
                        );
                    },
                );
            }
            RedrawEvent::CmdlineBlockShow(evt) => {
                evt.iter().for_each(|show| {
                    state.cmdline.show_block(show, &state.hl_defs);
                });
            }
            RedrawEvent::CmdlineBlockAppend(evt) => {
                evt.iter().for_each(|line| {
                    state.cmdline.block_append(line, &state.hl_defs);
                });
            }
            RedrawEvent::CmdlineBlockHide() => {
                state.cmdline.hide_block();
            }
            RedrawEvent::WildmenuShow(evt) => {
                evt.iter().for_each(|items| {
                    state.cmdline.wildmenu_show(&items.0);
                });
            }
            RedrawEvent::WildmenuHide() => {
                state.cmdline.wildmenu_hide();
            }
            RedrawEvent::WildmenuSelect(evt) => {
                evt.iter().for_each(|item| {
                    state.cmdline.wildmenu_select(*item);
                });
            }
            RedrawEvent::WindowPos(evt) => {
                evt.into_iter().for_each(|evt| {
                    let win = window.get_window().unwrap();
                    let windows_container = state.windows_container.clone();

                    let grid = state.grids.get(&evt.grid).unwrap();
                    let window =
                        state.windows.entry(evt.grid).or_insert_with(|| {
                            Window::new(
                                evt.win.clone(),
                                windows_container,
                                &grid,
                            )
                        });

                    let grid_metrics =
                        state.grids.get(&1).unwrap().get_grid_metrics();
                    let x = evt.start_col * grid_metrics.cell_width;
                    let y = evt.start_row * grid_metrics.cell_height;
                    let width = evt.width * grid_metrics.cell_width;
                    let height = evt.height * grid_metrics.cell_height;

                    window.set_position(x, y, width, height);
                    window.show();

                    grid.resize(&win, evt.width, evt.height);
                });
            }
            RedrawEvent::WindowFloatPos(evt) => {
                evt.iter().for_each(|evt| {
                    let anchor_grid =
                        state.grids.get(&evt.anchor_grid).unwrap();

                    let (x_offset, y_offset) = {
                        let anchor_window =
                            state.windows.get(&evt.anchor_grid).unwrap();
                        (anchor_window.x, anchor_window.y)
                    };

                    let grid = state.grids.get(&evt.grid).unwrap();
                    let windows_float_container =
                        state.windows_float_container.clone();
                    let window =
                        state.windows.entry(evt.grid).or_insert_with(|| {
                            println!("Create new float window");
                            Window::new(
                                evt.win.clone(),
                                windows_float_container,
                                &grid,
                            )
                        });

                    // TODO(ville): Move window from possible container to float container.

                    let anchor_metrics = anchor_grid.get_grid_metrics();
                    let grid_metrics = grid.get_grid_metrics();

                    let x =
                        x_offset + anchor_metrics.cell_width * evt.anchor_col;
                    let y =
                        y_offset + anchor_metrics.cell_height * evt.anchor_row;
                    let width = grid_metrics.cols * grid_metrics.cell_width;
                    let height = grid_metrics.rows * grid_metrics.cell_height;

                    window.set_position(x, y, width, height);
                    window.show();
                });
            }
            RedrawEvent::WindowHide(evt) => {
                evt.iter().for_each(|grid_id| {
                    state.windows.get(&grid_id).unwrap().hide();
                });
            }
            RedrawEvent::WindowClose(evt) => {
                evt.iter().for_each(|grid_id| {
                    // TODO(ville): Make sure all resources are dropped from the window.
                    state.windows.remove(&grid_id).unwrap(); // Drop window.
                });
            }
            RedrawEvent::MsgSetPos(evt) =>  {
                evt.iter().for_each(|e| {
                    let grid = state.grids.get(&e.grid).unwrap();
                    state.msg_grid_frame.add(&grid.widget());

                    let metrics = grid.get_grid_metrics();
                    let w = metrics.cols * metrics.cell_width;
                    let h = metrics.rows * metrics.cell_height;

                    state.msg_grid_frame.set_size_request(w as i32, h as i32);
                    state.msg_grid_container.move_(&state.msg_grid_frame, 0, (metrics.cell_height * e.row) as i32);
                    state.msg_grid_container.show_all();
                });
            }
            RedrawEvent::Ignored(_) => (),
            RedrawEvent::Unknown(e) => {
                println!("Received unknown redraw event: {}", e);
            }
        }
    }
}

fn attach_grid_events(grid: &Grid, nvim: Rc<RefCell<Neovim>>) {
    let id = grid.id;
    // Mouse button press event.
    grid.connect_mouse_button_press_events(
        clone!(nvim => move |button, row, col| {
            let mut nvim = nvim.borrow_mut();
            nvim.input_mouse(&button.to_string(), "press", "", id, row as i64, col as i64).unwrap();

            Inhibit(false)
        }),
    );

    // Mouse button release events.
    grid.connect_mouse_button_release_events(
        clone!(nvim => move |button, row, col| {
            let mut nvim = nvim.borrow_mut();
            nvim.input_mouse(&button.to_string(), "release", "", id, row as i64, col as i64).unwrap();

            Inhibit(false)
        }),
    );

    // Mouse drag events.
    grid.connect_motion_events_for_drag(
        clone!(nvim => move |button, row, col| {
            let mut nvim = nvim.borrow_mut();
            nvim.input_mouse(&button.to_string(), "drag", "", id, row as i64, col as i64).unwrap();

            Inhibit(false)
        }),
    );

    // Scrolling events.
    grid.connect_scroll_events(clone!(nvim => move |dir, row, col| {
        let mut nvim = nvim.borrow_mut();
        nvim.input_mouse("wheel", &dir.to_string(), "", id, row as i64, col as i64).unwrap();

        Inhibit(false)
    }));
}

fn keyname_to_nvim_key(s: &str) -> Option<&str> {
    // Sourced from python-gui.
    match s {
        "slash" => Some("/"),
        "backslash" => Some("\\"),
        "dead_circumflex" => Some("^"),
        "at" => Some("@"),
        "numbersign" => Some("#"),
        "dollar" => Some("$"),
        "percent" => Some("%"),
        "ampersand" => Some("&"),
        "asterisk" => Some("*"),
        "parenleft" => Some("("),
        "parenright" => Some(")"),
        "underscore" => Some("_"),
        "plus" => Some("+"),
        "minus" => Some("-"),
        "bracketleft" => Some("["),
        "bracketright" => Some("]"),
        "braceleft" => Some("{"),
        "braceright" => Some("}"),
        "dead_diaeresis" => Some("\""),
        "dead_acute" => Some("\'"),
        "less" => Some("<"),
        "greater" => Some(">"),
        "comma" => Some(","),
        "period" => Some("."),
        "BackSpace" => Some("BS"),
        "Insert" => Some("Insert"),
        "Return" => Some("CR"),
        "Escape" => Some("Esc"),
        "Delete" => Some("Del"),
        "Page_Up" => Some("PageUp"),
        "Page_Down" => Some("PageDown"),
        "Enter" => Some("CR"),
        "ISO_Left_Tab" => Some("Tab"),
        "Tab" => Some("Tab"),
        "Up" => Some("Up"),
        "Down" => Some("Down"),
        "Left" => Some("Left"),
        "Right" => Some("Right"),
        "Home" => Some("Home"),
        "End" => Some("End"),
        "F1" => Some("F1"),
        "F2" => Some("F2"),
        "F3" => Some("F3"),
        "F4" => Some("F4"),
        "F5" => Some("F5"),
        "F6" => Some("F6"),
        "F7" => Some("F7"),
        "F8" => Some("F8"),
        "F9" => Some("F9"),
        "F10" => Some("F10"),
        "F11" => Some("F11"),
        "F12" => Some("F12"),
        _ => None,
    }
}

fn event_to_nvim_input(e: &gdk::EventKey) -> Option<String> {
    let mut input = String::from("");

    let keyval = e.get_keyval();
    let keyname = gdk::keyval_name(keyval)?;

    let state = e.get_state();

    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        input.push_str("S-");
    }
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        input.push_str("C-");
    }
    if state.contains(gdk::ModifierType::MOD1_MASK) {
        input.push_str("A-");
    }

    if keyname.chars().count() > 1 {
        let n = keyname_to_nvim_key(keyname.as_str())?;
        input.push_str(n);
    } else {
        input.push(gdk::keyval_to_unicode(keyval)?);
    }

    Some(format!("<{}>", input))
}
