{
  description = "basic rust development evnvironment";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = {nixpkgs, rust-overlay, ...}:
      let 
        system = "x86_64-linux";
        pkgs = import nixpkgs { inherit system; overlays = [ rust-overlay.overlays.default ]; };
      in
    with pkgs; {
      devShells.${system}.default = mkShell.override { stdenv = pkgs.clangStdenv; } rec {

          packages = [
            (rust-bin.stable.latest.default.override {
              extensions = [ "rust-src" "rust-analyzer" ];
            })
            cargo-flamegraph
            samply
            lldb
          ];
          
          nativeBuildInputs = with pkgs; [
            pkg-config
            cmake
            mesa
          ];
          
          buildInputs = with pkgs; [
            ffmpeg
            fontconfig
            freetype

            vulkan-headers
            vulkan-loader
            libGL

            libxkbcommon
            # WINIT_UNIX_BACKEND=wayland
            wayland

            # WINIT_UNIX_BACKEND=x11
            xorg.libXcursor
            xorg.libXrandr
            xorg.libXi
            xorg.libX11
          ];

          LD_LIBRARY_PATH = "${pkgs.lib.makeLibraryPath buildInputs}:${addDriverRunpath.driverLink}/lib";
          LIBCLANG_PATH = with pkgs; "${llvmPackages.libclang.lib}/lib";
        };

      formatter.x86_64-linux = legacyPackages.${system}.nixpkgs-fmt;
    };
}

