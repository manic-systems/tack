# SPDX-License-Identifier: EUPL-1.2

{
  inputs.shared = { };

  # strict in an input that follows `parent/dep`, as a flake-parts mkFlake is
  outputs = { shared, ... }: builtins.seq shared { inherit shared; };
}
