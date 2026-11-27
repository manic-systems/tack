# SPDX-License-Identifier: EUPL-1.2

{
  # follows into this flake's tack pins must not eval outputs
  outputs = _: throw "tack: lazy-flake's outputs were forced";
}
