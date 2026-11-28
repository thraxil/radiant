{
  description = "radiant - Rust development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Cask (content-addressed storage cluster), built here so scripts/test-cask.sh can run
    # real nodes. `nix flake update cask` picks up newer versions.
    cask = {
      url = "github:thraxil/cask";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, cask }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        caskPkg = pkgs.buildGoModule {
          pname = "cask";
          version = cask.shortRev or "dirty";
          src = cask;
          vendorHash = null; # the repo vendors its dependencies
          doCheck = false;
        };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.cargo-watch
            pkgs.cargo-nextest
            pkgs.cargo-edit
            pkgs.pkg-config
            # Local S3-compatible server for scripts/test-s3.sh
            pkgs.versitygw
            # Cask nodes for scripts/test-cask.sh
            caskPkg
            # tools/migrate-reticulum
            pkgs.python3
          ];

          RUST_BACKTRACE = "1";
        };

        packages.cask = caskPkg;

        formatter = pkgs.nixpkgs-fmt;
      });
}
