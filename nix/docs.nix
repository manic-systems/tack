# SPDX-License-Identifier: EUPL-1.2

# tack book
{
  lib,
  ndg,
  nixosOptionsDoc,
  runCommandLocal,
  writers,
  self,
}:
let
  modulePath = toString ./module.nix;
  blob = "https://github.com/manic-systems/tack/blob/${self.rev or "main"}";

  # eval only the module
  options =
    (lib.evalModules {
      modules = [
        (import ./module.nix self)
        {
          _file = modulePath;
          options._module.args = lib.mkOption { internal = true; };
          config._module = {
            check = false;
            args.pkgs = { };
          };
        }
      ];
    }).options;

  optionsDoc = nixosOptionsDoc {
    inherit options;
    warningsAreErrors = false;
    variablelistId = "tack-options";
    transformOptions =
      opt:
      opt
      // {
        visible = opt.visible != false && lib.hasPrefix "programs.tack" opt.name;
        declarations = map (decl: {
          name = "<tack/nix/module.nix>";
          url = "${blob}/nix/module.nix";
        }) opt.declarations;
      };
  };

  paths = writers.writeTOML "ndg-paths.toml" {
    input_dir = ../docs;
    module_options = "${optionsDoc.optionsJSON}/share/doc/nixos/options.json";
    revision = self.rev or "main";
    script_paths = [ "assets/current-page.js" ];
    stylesheet_paths = [ ../docs/theme/current-page.css ];
  };
in
runCommandLocal "tack-docs"
  {
    nativeBuildInputs = [ ndg ];
    meta.description = "the tack book";
  }
  ''
    # ndg links a script by its configured path, so it has to sit where the
    # output serves it
    mkdir assets && cp ${../docs/theme/current-page.js} assets/current-page.js

    # a later config file resets what it leaves out to the defaults, so the
    # book's own settings go last
    ndg --config-file ${paths} --config-file ${../docs/ndg.toml} \
      html --jobs $NIX_BUILD_CORES --output-dir $out
  ''
