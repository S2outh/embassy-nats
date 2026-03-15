{
  description = "embassy flake";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix/monthly";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, fenix, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let 
        pkgs = import nixpkgs {
          inherit system;
          overlays = [
            fenix.overlays.default 
          ];
        };
      in
      {
        devShells.default =
        let
          toolchain = pkgs.fenix.complete;
          std-lib = pkgs.fenix.targets.thumbv7em-none-eabihf.latest;
          rust-pkgs = pkgs.fenix.combine [
            toolchain.rustc-unwrapped
            toolchain.rust-src
            toolchain.cargo
            toolchain.rustfmt
            toolchain.clippy
            std-lib.rust-std
          ];
        in
        pkgs.mkShell {
          buildInputs = with pkgs; [
            rust-pkgs
            rust-analyzer-nightly

            # extra cargo tools
            cargo-edit
            cargo-expand

            # for flashing
            probe-rs-tools
          ];

          # set the rust src for rust_analyzer
          RUST_SRC_PATH = "${rust-pkgs}/lib/rustlib/src/rust/library";
					# set default defmt log level
					DEFMT_LOG = "info";
        };
      }
    );
}
