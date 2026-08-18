{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      self,
      flake-parts,
      nixpkgs,
      rust-overlay,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      # The two home-manager modules: porthole-daemon for the macOS side,
      # porthole-remote for NixOS remotes. `self` is passed so each module
      # can default its package to this flake's build.
      flake.homeModules = {
        porthole-daemon = import ./nix/home-darwin.nix self;
        porthole-remote = import ./nix/home-remote.nix self;
      };

      perSystem =
        { system, ... }:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };

          rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          # Build with the devshell's toolchain so what we ship is what
          # we test.
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rust;
            rustc = rust;
          };

          # daemon = true  → the full build (macOS local host).
          # daemon = false → remote client only: the `daemon` cargo
          # feature is disabled and tokio is never compiled.
          mkPorthole =
            {
              daemon ? true,
            }:
            rustPlatform.buildRustPackage {
              pname = "porthole";
              version = "0.0.0";
              src = ./.;
              # Deps are vendored from the lockfile's own checksums
              # (importCargoLock); no cargoHash dance while every dep
              # comes from crates.io.
              cargoLock.lockFile = ./Cargo.lock;
              buildNoDefaultFeatures = !daemon;
              # Tests are the smoke rigs in scripts/, not cargo test.
              doCheck = false;
              # buildRustPackage overrides the release profile with env
              # (notably panic=unwind); pin ours back.
              CARGO_PROFILE_RELEASE_OPT_LEVEL = "z";
              CARGO_PROFILE_RELEASE_LTO = "fat";
              CARGO_PROFILE_RELEASE_CODEGEN_UNITS = "1";
              CARGO_PROFILE_RELEASE_PANIC = "abort";
              CARGO_PROFILE_RELEASE_STRIP = "true";
              postInstall = ''
                # stdenv's strip phase doesn't fire for us; do it.
                strip $out/bin/porthole
                ln -s $out/bin/porthole $out/bin/ph
                # The herdr plugin manifest. `herdr plugin link
                # <this dir>` reads herdr-plugin.toml from it.
                mkdir -p $out/share/porthole
                cp ${./herdr-plugin.toml} $out/share/porthole/herdr-plugin.toml
              ''
              + pkgs.lib.optionalString (!daemon) ''
                # Busybox-style argv[0] dispatch makes these shims:
                # `open` for macOS-style callers and $BROWSER, `pbcopy`
                # for the clipboard bridge.
                ln -s $out/bin/porthole $out/bin/open
                ln -s $out/bin/porthole $out/bin/pbcopy
              '';
              meta.mainProgram = "porthole";
            };
        in
        {
          formatter = pkgs.nixfmt-tree;

          packages = {
            default = mkPorthole { };
            porthole = mkPorthole { };
            porthole-remote = mkPorthole { daemon = false; };
          };

          devShells.default = pkgs.mkShell {
            packages = with pkgs; [
              rust
              # Test tooling: drive daemon sockets directly (nc -U).
              # Declared here because the system netcat variant is a lottery.
              netcat
              # smoke-e2e runs a user-mode sshd.
              openssh
              # `porthole status`/`tunnel kill` list mux forwards via
              # lsof on the ssh master's pid.
              lsof
              # Task runner + GitHub Actions hygiene tooling. CI runs
              # both of these in the security workflow.
              just
              pinact
              zizmor
            ];
          };
        };
    };
}
