# SPDX-License-Identifier: EUPL-1.2

{
  description = "flake-like toml nix pins, lazily fetched and transformed";

  outputs =
    { self }:
    let
      inputs = import ./inputs.nix;
      inherit (inputs) nixpkgs fenix;
      inherit (nixpkgs) lib;
      forAllSystems = lib.genAttrs lib.systems.flakeExposed;
      pkgsFor = system: nixpkgs.legacyPackages.${system};

      # wild + clang are only used on Linux tier-1 arches
      hasWild = plat: plat.isLinux && (plat.isx86_64 || plat.isAarch64);

      nativeDeps =
        pkgs:
        [ pkgs.pkg-config ]
        ++ lib.optionals (hasWild pkgs.stdenv.hostPlatform) [
          pkgs.wild
          pkgs.clang
        ];
      linkDeps = pkgs: [
        pkgs.openssl
        pkgs.libgit2
        pkgs.libssh2
        pkgs.zlib
      ];
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = self.packages.${system}.tack;

          tack = pkgs.rustPlatform.buildRustPackage {
            pname = "tack";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = nativeDeps pkgs;
            buildInputs = linkDeps pkgs;

            # link nixpkgs c libs, no vendored copies
            env = {
              LIBGIT2_NO_VENDOR = 1;
              OPENSSL_NO_VENDOR = 1;
            };

            meta = {
              description = "flake-like toml nix pins, lazily fetched and transformed";
              mainProgram = "tack";
            };
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          nightlyRustfmt = fenix.packages.${system}.latest.rustfmt;
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.rust-analyzer
            ]
            ++ nativeDeps pkgs
            ++ linkDeps pkgs;

            env = {
              RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
              LIBGIT2_NO_VENDOR = 1;
              OPENSSL_NO_VENDOR = 1;
            };
          };

          # `nix develop .#fmt` formats on entry.
          fmt = pkgs.mkShellNoCC {
            packages = [
              pkgs.cargo
              nightlyRustfmt
              pkgs.taplo
              pkgs.nixfmt
            ];
            shellHook = ''
              cargo fmt
              taplo fmt
              find . -name '*.nix' -not -path './target/*' -exec nixfmt {} +
            '';
          };
        }
      );

      checks = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          nightlyRustfmt = fenix.packages.${system}.latest.rustfmt;
        in
        {
          default = self.checks.${system}.fmt;
          fmt =
            pkgs.runCommand "tack-fmt-check"
              {
                nativeBuildInputs = [
                  pkgs.cargo
                  nightlyRustfmt
                  pkgs.taplo
                  pkgs.nixfmt
                ];
                src = ./.;
              }
              ''
                cp -r $src ./tree
                chmod -R +w ./tree
                cd ./tree
                cargo fmt -- --check
                taplo fmt --check
                find . -name '*.nix' -exec nixfmt --check {} +
                touch $out
              '';
        }
      );
    };
}
