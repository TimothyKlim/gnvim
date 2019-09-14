use gtk::prelude::*;

use neovim_lib::neovim_api::Window as NvimWindow;

use ui::grid::Grid;

pub struct MsgWindow {
    fixed: gtk::Fixed,
    frame: gtk::Frame,
}

impl MsgWindow {
    pub fn new(fixed: gtk::Fixed) -> Self {
        let frame = gtk::Frame::new(None);

        fixed.put(&frame, 0, 0);

        Self {
            fixed,
            frame,
        }
    }

    pub fn set_pos(&self, grid: &Grid, row: u64) {
        // TODO(ville): Remove grid from its parent?
        self.frame.add(&grid.widget());

        let metrics = grid.get_grid_metrics();
        let w = metrics.cols * metrics.cell_width;
        let h = metrics.rows * metrics.cell_height;
        self.frame.set_size_request(w as i32, h  as i32);

        self.fixed.move_(&self.frame, 0, (metrics.cell_height * row) as i32);
        self.fixed.show_all();
    }
}

pub struct Window {
    fixed: gtk::Fixed,
    frame: gtk::Frame,

    pub x: u64,
    pub y: u64,

    /// Currently shown's grid id.
    pub grid_id: i64,
    pub nvim_win: NvimWindow,
}

impl Window {
    pub fn new(win: NvimWindow, fixed: gtk::Fixed, grid: &Grid) -> Self {
        let frame = gtk::Frame::new(None);
        fixed.put(&frame, 0, 0);

        let widget = grid.widget();
        frame.add(&widget);

        Self {
            fixed,
            frame,
            grid_id: grid.id,
            nvim_win: win,
            x: 0,
            y: 0,
        }
    }

    pub fn set_position(&mut self, x: u64, y: u64, w: u64, h: u64) {
        self.x = x;
        self.y = y;
        self.fixed.move_(&self.frame, x as i32, y as i32);

        self.frame.set_size_request(w as i32, h as i32);
    }

    pub fn show(&self) {
        self.frame.show_all();
    }

    pub fn hide(&self) {
        self.frame.hide();
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // TODO(ville): Test that we release all resources.
        if let Some(child) = self.frame.get_child() {
            // We dont want to destroy the child widget, so just remove the child from our
            // container.
            self.frame.remove(&child);
        }
        self.frame.destroy();
    }
}
