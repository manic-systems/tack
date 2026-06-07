# SPDX-License-Identifier: EUPL-1.2

{
  description = "flake-like toml nix pins, lazily fetched and transformed";

  outputs =
    { self, ... }@args:
    let
      inputs = (import ./.tack) {
        overrides = args.tackOverrides or { };
      };
      inherit (inputs) nixpkgs fenix;
      inherit (nixpkgs) lib;
      forAllSystems = lib.genAttrs (lib.systems.doubles.linux ++ lib.systems.doubles.darwin);
      pkgsFor = system: nixpkgs.legacyPackages.${system} or (import nixpkgs { inherit system; });

      # wild + clang are only used on Linux tier-1 arches
      hasWild = plat: plat.isLinux && (plat.isx86_64 || plat.isAarch64);

      nativeDeps =
        pkgs:
        lib.optionals (hasWild pkgs.stdenv.hostPlatform) [
          pkgs.wild
          pkgs.clang
        ];
    in
    {
      templates.default = {
        path = ./templates/default;
        description = "tack-wired flake, recomposable via tackOverrides";
      };

      nixosModules.default = import ./nix/module.nix self;

      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = self.packages.${system}.tack;

          tack = pkgs.callPackage ./nix/package.nix { };
        }
      );

      apps = forAllSystems (
        system:
        let
          tack = self.packages.${system}.tack;
        in
        {
          default = {
            type = "app";
            program = lib.getExe tack;
            inherit (tack) meta;
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
              nightlyRustfmt
              pkgs.rust-analyzer
            ]
            ++ nativeDeps pkgs;

            env = {
              RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            };
          };

          # `nix develop .#fmt` formats on entry
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
          default = pkgs.linkFarmFromDrvs "tack-checks" [
            self.checks.${system}.fmt
            self.checks.${system}.clippy
          ];
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
          clippy = self.packages.${system}.tack.overrideAttrs (old: {
            pname = "tack-clippy";
            nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.clippy ];
            buildPhase = ''
              runHook preBuild
              cargo clippy --all-targets --offline -- -D warnings
              runHook postBuild
            '';
            checkPhase = "true";
            doCheck = false;
            installPhase = ''
              runHook preInstall
              touch $out
              runHook postInstall
            '';
          });
        }
      );
    };
}
