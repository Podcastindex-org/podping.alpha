{
  description = "Podping.alpha - gossip-listener";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default;

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Include gossip-listener and dtt-fork (local path dependency).
        # The source root stays at the repo level so that the relative
        # path `../dtt-fork` in gossip-listener/Cargo.toml resolves.
        gossipListenerSrc = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter =
            path: type:
            let
              rel = pkgs.lib.removePrefix (toString ./. + "/") (toString path);
              inScope =
                pkgs.lib.hasPrefix "gossip-listener/" rel
                || pkgs.lib.hasPrefix "dtt-fork/" rel
                || rel == "gossip-listener"
                || rel == "dtt-fork";
              # dtt-fork uses include_str!("../README.md") so .md files
              # must survive filtering alongside normal Cargo sources.
              isMarkdown = pkgs.lib.hasSuffix ".md" path;
            in
            inScope && ((craneLib.filterCargoSources path type) || isMarkdown);
        };

        # Vendor all dependencies (including the iroh-gossip git dep)
        # from the gossip-listener lockfile.
        cargoVendorDir = craneLib.vendorCargoDeps {
          src = gossipListenerSrc;
          cargoLock = ./gossip-listener/Cargo.lock;
          cargoToml = ./gossip-listener/Cargo.toml;
        };

        commonArgs = {
          src = gossipListenerSrc;
          inherit cargoVendorDir;

          # Crane needs these to locate the manifest and lockfile since
          # they are not at the source root.
          cargoToml = ./gossip-listener/Cargo.toml;
          cargoLock = ./gossip-listener/Cargo.lock;

          strictDeps = true;

          # Point cargo at the correct manifest.
          cargoExtraArgs = "--manifest-path gossip-listener/Cargo.toml";

          # Crane places the vendored Cargo.lock at the source root during
          # patchPhase, but cargo with --manifest-path looks for it next to
          # the Cargo.toml.  Symlink it into the crate directory so both
          # cargo and the git-source-replacement lockfile check are satisfied.
          preBuild = ''
            if [ -f Cargo.lock ] && [ ! -f gossip-listener/Cargo.lock ]; then
              ln -s "$(pwd)/Cargo.lock" gossip-listener/Cargo.lock
            fi
          '';

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];

          buildInputs =
            with pkgs;
            [
              openssl
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
              pkgs.darwin.apple_sdk.frameworks.Security
              pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
            ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        gossipListener = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
          }
        );
      in
      {
        packages = {
          gossip-listener = gossipListener;
          default = gossipListener;
        };

        apps.default = flake-utils.lib.mkApp {
          drv = gossipListener;
          name = "gossip-listener";
        };

        devShells.default = craneLib.devShell {
          inputsFrom = [ gossipListener ];

          packages = with pkgs; [
            cargo-watch
            rust-analyzer
          ];
        };
      }
    );
}
