# SPDX-License-Identifier: EUPL-1.2

{
  inputs.child = { };
  inputs.dep = { };

  outputs = { child, ... }: builtins.seq child { inherit child; };
}
