# SPDX-License-Identifier: EUPL-1.2
# tack-managed resolver. delete this line to take ownership; tack will leave it alone afterwards.

let
  pins = builtins.fromTOML (builtins.readFile ./pins.toml);
  lock = builtins.fromJSON (builtins.readFile ./pins.lock.json);
  all_follow_raw = pins.all_follow or { };

  # flatten `target = [aliases]` rows alongside `alias = "target"` rows
  all_follow = builtins.foldl' (
    acc: key:
    let
      val = all_follow_raw.${key};
    in
    if builtins.isList val then
      acc
      // {
        ${key} = key;
      }
      // builtins.listToAttrs (
        map (a: {
          name = a;
          value = key;
        }) val
      )
    else if builtins.isString val then
      acc // { ${key} = val; }
    else
      acc
  ) { } (builtins.attrNames all_follow_raw);

  fetchPin = name: builtins.fetchTree lock.${name};

  fetchFixed =
    name: entry:
    let
      raw = derivation {
        inherit name;
        builder = "builtin:fetchurl";
        system = "builtin";
        url = entry.url;
        outputHash = entry.sha256;
        outputHashAlgo = "sha256";
        outputHashMode = "flat";
      };
      unpacked = derivation {
        inherit name;
        builder = "builtin:unpack-channel";
        system = "builtin";
        src = raw;
        channelName = name;
      };
    in
    if (entry.unpack or "file") == "tarball" then unpacked.outPath + "/" + name else raw.outPath;

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
      pinType =
        if pin ? type then
          pin.type
        else if pin.flake or true then
          "flake"
        else
          "fetch";
      subdir = if pin ? dir then "/" + pin.dir else "";
    in
    if pinType == "fixed" then
      fetchFixed name lock.${name}
    else
      let
        sourceInfo = fetchPin name;
      in
      if pinType == "flake" then evalTopFlake sourceInfo pin else sourceInfo.outPath + subdir;

  declared = pins.inputs or { };

  # any lock entry without a declared [inputs] mate is an auto-dedup synthetic
  # written by `tack update` for [all_follow] targets that aren't pinned
  # top-level. fall back to the bare source tree if the fetched tree has no
  # flake.nix (so `flake = false` consumers get a usable sourceInfo)
  autoNames = builtins.filter (n: !(declared ? ${n})) (builtins.attrNames lock);
  autoPin =
    name:
    let
      sourceInfo = fetchPin name;
    in
    if builtins.pathExists (sourceInfo.outPath + "/flake.nix") then
      evalTopFlake sourceInfo { }
    else
      sourceInfo;

  self =
    (builtins.mapAttrs loadPin declared)
    // builtins.listToAttrs (
      map (name: {
        inherit name;
        value = autoPin name;
      }) autoNames
    );
in
self
