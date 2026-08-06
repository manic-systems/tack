# SPDX-License-Identifier: EUPL-1.2

let
  call =
    {
      overrides ? { },
    }:
    {
      value = "legacy";
    }
    // overrides;
in
(call { }) // { __functor = _: call; }
