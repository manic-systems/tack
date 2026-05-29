{
  description = "";

  # `tack init` populates ./.tack with your pins.
  outputs =
    { self, ... }@args:
    let
      inputs = (import ./.tack) { overrides = args.tackOverrides or { }; };
    in
    { };
}
