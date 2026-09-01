{
  description = "embassy flake";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    systems.url = "github:nix-systems/default";
    flake-parts.url = "github:hercules-ci/flake-parts";
    fenix.url = "github:nix-community/fenix/monthly";
  };

  outputs = inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } ({ ... }: {
      systems = import inputs.systems;
      perSystem = { pkgs, system, inputs', ... }: 
      let
        fpkgs = inputs'.fenix.packages;
        profile = fpkgs.complete;
        std-lib = fpkgs.targets.thumbv7em-none-eabihf.latest;
        rust-analyzer-nightly = fpkgs.rust-analyzer;
        rust-toolchain = fpkgs.combine [
          profile.rustc
          profile.rust-src
          profile.cargo
          profile.rustfmt
          profile.clippy
          profile.llvm-tools
          std-lib.rust-std
        ];
      in {
        devShells.default =
        pkgs.mkShell {
          buildInputs = with pkgs; [
            rust-toolchain
            rust-analyzer-nightly

            # extra cargo tools
            cargo-edit
            cargo-expand
            cargo-show-asm
            cargo-binutils
          ];

          # set the rust src for rust_analyzer
          RUST_SRC_PATH = "${rust-toolchain}/lib/rustlib/src/rust/library";
        };
      };
    });
}

