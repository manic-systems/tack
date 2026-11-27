# SPDX-License-Identifier: EUPL-1.2

let
  resolver = import ../../../../.tack;
  call = args: resolver (args // { resolverDir = ./.; });
in
(call { }) // { __functor = _: call; }
