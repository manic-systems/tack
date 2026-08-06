# SPDX-License-Identifier: EUPL-1.2

{
  inputs.scoped = { };

  outputs =
    { self, ... }@args:
    let
      pins = (import ./.tack) {
        overrides = args.tackOverrides or { };
      };
    in
    {
      nested = pins.mid;
    };
}
