# SPDX-License-Identifier: EUPL-1.2

let
  resolver = import ../../../../.tack;
  call =
    args:
    resolver (args // { resolverDir = ./.; })
    // {
      policy = (args.overrides or { }).__tack_policy or { };
    };
in
(call { }) // { __functor = _: call; }
