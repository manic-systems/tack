# SPDX-License-Identifier: EUPL-1.2

# nixosModules.default, partially applied with the flake's `self` so the package
# default can point at the flake's own build
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.tack;
in
{
  options.programs.tack = {
    enable = lib.mkEnableOption "tack, flake-like toml nix pins";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.tack;
      defaultText = lib.literalExpression "tack.packages.\${system}.tack";
      description = "the tack package to install.";
    };

    nixConfTokens = lib.mkEnableOption ''
      tack reading `access-tokens` from nix.conf when comparing forge revisions.
      off by default: it widens which credentials tack may replay to a forge
      beyond the ones in the environment
    '';
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    # the rust side gates the scrape on this var; the option just sets it
    environment.sessionVariables = lib.mkIf cfg.nixConfTokens {
      TACK_NIX_CONF_TOKENS = "1";
    };
  };
}
