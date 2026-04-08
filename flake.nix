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
    flake-utils.lib.eachSystem [ "x86_64-linux" ] (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default;

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Custom filter: Keep markdown files (for include_str!("README.md")) AND Cargo files
        markdownFilter = path: _type: builtins.match ".*md$" path != null;
        cargoOrMarkdown = path: type: (markdownFilter path type) || (craneLib.filterCargoSources path type);

        # Include the entire repository source with the custom filter
        workspaceSrc = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = cargoOrMarkdown;
        };

        gossipListenerCommonArgs = {
          src = workspaceSrc;
          strictDeps = true;

          # Point crane to the nested Cargo configuration files
          cargoToml = ./gossip-listener/Cargo.toml;
          cargoLock = ./gossip-listener/Cargo.lock;

          # Shift the build context into the gossip-listener folder after unpacking
          postUnpack = ''
            export sourceRoot=$sourceRoot/gossip-listener
          '';

          # Git dependencies cause lock file drift after vendoring
          cargoExtraArgs = "--offline";

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

        gossipListenerCargoArtifacts = craneLib.buildDepsOnly gossipListenerCommonArgs;

        gossipListener = craneLib.buildPackage (
          gossipListenerCommonArgs
          // {
            cargoArtifacts = gossipListenerCargoArtifacts;
          }
        );

      in
      {
        packages = {
          gossip-listener = gossipListener;
          default = gossipListener;
        };

        apps = {
          default = flake-utils.lib.mkApp {
            drv = gossipListener;
            name = "gossip-listener";
          };
          gossip-listener = flake-utils.lib.mkApp {
            drv = gossipListener;
            name = "gossip-listener";
          };
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
