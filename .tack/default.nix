# SPDX-License-Identifier: EUPL-1.2
# tack-managed resolver. delete this line to take ownership; tack will leave it alone afterwards.

let
  pins = builtins.fromTOML (builtins.readFile ./pins.toml);
  lock = builtins.fromJSON (builtins.readFile ./pins.lock.json);
  all_follow = pins.all_follow or { };

  fetchPin = name: builtins.fetchTree lock.${name};

  resolveSpec = upLock: spec: if builtins.isList spec then walkPath upLock upLock.root spec else spec;

  walkPath =
    upLock: nodeName: path:
    if path == [ ] then
      nodeName
    else
      walkPath upLock (resolveSpec upLock upLock.nodes.${nodeName}.inputs.${builtins.head path}) (
        builtins.tail path
      );

  mkCallerInputs =
    upLock: nodeName: rawInputs: levelFollows: deepFollows:
    let
      overrides = builtins.mapAttrs (_: target: self.${target}) levelFollows;
    in
    builtins.mapAttrs (
      n: _decl:
      if overrides ? ${n} then
        overrides.${n}
      else if upLock != null then
        let
          ref =
            (upLock.nodes.${nodeName}.inputs or { }).${n}
              or (throw "tack/inputs.nix: input '${n}' declared but not in flake.lock node '${nodeName}'");
          childName = resolveSpec upLock ref;
          childNode = upLock.nodes.${childName};
          childSrc = builtins.fetchTree childNode.locked;
        in
        if childNode.flake or true then evalTransitive upLock childName childSrc deepFollows else childSrc
      else
        throw "tack/inputs.nix: no flake.lock; cannot resolve input '${n}'"
    ) rawInputs;

  evalTransitive =
    upLock: nodeName: sourceInfo: follows:
    let
      raw = import (sourceInfo.outPath + "/flake.nix");
      callerInputs = mkCallerInputs upLock nodeName (raw.inputs or { }) follows follows;
      outputs = raw.outputs (callerInputs // { self = result; });
      result =
        outputs
        // sourceInfo
        // {
          outPath = sourceInfo.outPath;
          inputs = callerInputs;
          inherit outputs;
          inherit sourceInfo;
          _type = "flake";
        };
    in
    result;

  evalTopFlake =
    sourceInfo: pin:
    let
      flakeDir = sourceInfo.outPath + (if pin ? dir then "/" + pin.dir else "");
      raw = import (flakeDir + "/flake.nix");
      upLockPath = flakeDir + "/flake.lock";
      upLock =
        if builtins.pathExists upLockPath then builtins.fromJSON (builtins.readFile upLockPath) else null;

      exclude_follow = pin.exclude_follow or [ ];
      explicit_follows = pin.follows or { };
      all_follow_rules = builtins.removeAttrs all_follow exclude_follow;
      combined_follows = explicit_follows // all_follow_rules;

      rootNode = if upLock != null then upLock.root else null;
      callerInputs = mkCallerInputs upLock rootNode (raw.inputs or { }) combined_follows all_follow_rules;

      outputs = raw.outputs (callerInputs // { self = result; });
      result =
        outputs
        // sourceInfo
        // {
          outPath = flakeDir;
          inputs = callerInputs;
          inherit outputs;
          inherit sourceInfo;
          _type = "flake";
        };
    in
    result;

  loadPin =
    name: pin:
    let
      sourceInfo = fetchPin name;
      subdir = if pin ? dir then "/" + pin.dir else "";
    in
    if pin.flake or true then evalTopFlake sourceInfo pin else sourceInfo.outPath + subdir;

  self = builtins.mapAttrs loadPin pins.inputs;
in
self
