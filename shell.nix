let
  mozillaOverlay = import (builtins.fetchTarball "https://github.com/mozilla/nixpkgs-mozilla/archive/master.tar.gz");
  pkgs = import <nixpkgs> { overlays = [ mozillaOverlay ]; };
in
  pkgs.mkShell {
    nativeBuildInputs = with pkgs; [ cmake pkgconfig ];
    buildInputs = with pkgs; [ latest.rustChannels.stable.rust clang pkgconfig openssl webkitgtk ];

    shellHook = ''
      export PATH=$PWD/target/release:$PATH
      export TERM=xterm
      export LIBCLANG_PATH="${pkgs.llvmPackages.libclang}/lib";
      '';
  }
