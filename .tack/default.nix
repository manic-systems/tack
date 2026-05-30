# SPDX-License-Identifier: EUPL-1.2
# tack-managed resolver. delete this line to take ownership; tack will leave it alone afterwards.

let
  inherit (builtins)
    attrNames
    attrValues
    filter
    foldl'
    fromJSON
    head
    isList
    isString
    listToAttrs
    mapAttrs
    pathExists
    readFile
    tail
    ;

  pins = fromTOML (readFile ./pins.toml);
  lock = fromJSON (readFile ./pins.lock.json);
  all_follow_raw = pins.all_follow or { };

  # flatten `target = [aliases]` rows alongside `alias = "target"` rows
  all_follow = foldl' (
    acc: key:
    let
      val = all_follow_raw.${key};
    in
    if isList val then
      acc
      // {
        ${key} = key;
      }
      // listToAttrs (
        map (a: {
          name = a;
          value = key;
        }) val
      )
    else if isString val then
      acc // { ${key} = val; }
    else
      acc
  ) { } (attrNames all_follow_raw);

  fetchPin = name: fetchTree lock.${name};

  fetchFixed =
    name: entry:
    let
      raw = derivation {
        inherit name;
        inherit (entry) url;
        builder = "builtin:fetchurl";
        system = "builtin";
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

  resolveSpec = upLock: spec: if isList spec then walkPath upLock upLock.root spec else spec;

  walkPath =
    upLock: nodeName: path:
    if path == [ ] then
      nodeName
    else
      walkPath upLock (resolveSpec upLock upLock.nodes.${nodeName}.inputs.${head path}) (tail path);

  mkCallerInputs =
    upLock: nodeName: rawInputs: levelFollows: deepFollows:
    let
      overrides = mapAttrs (_: target: self.${target}) levelFollows;
    in
    mapAttrs (
      n: _:
      overrides.${n} or (
        if upLock != null then
          let
            ref =
              (upLock.nodes.${nodeName}.inputs or { }).${n}
                or (throw "tack/inputs.nix: input '${n}' declared but not in flake.lock node '${nodeName}'");
            childName = resolveSpec upLock ref;
            childNode = upLock.nodes.${childName};
            childSrc = fetchTree childNode.locked;
          in
          if childNode.flake or true then evalTransitive upLock childName childSrc deepFollows else childSrc
        else
          throw "tack/inputs.nix: no flake.lock; cannot resolve input '${n}'"
      )
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
          inherit (sourceInfo) outPath;
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
      upLock = if pathExists upLockPath then fromJSON (readFile upLockPath) else null;

      exclude_follow = pin.exclude_follow or [ ];
      explicit_follows = pin.follows or { };
      all_follow_rules = removeAttrs all_follow exclude_follow;
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
      pinType = pin.type or (if pin.flake or true then "flake" else "fetch");
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

  # undeclared lock entries are auto-dedup synthetics only when they are
  # referenced as [all_follow] targets. stale locks left after hand-editing
  # pins.toml are ignored, and can be cleaned with `tack rm <name>`.
  autoTargets = listToAttrs (
    map (target: {
      name = target;
      value = true;
    }) (attrValues all_follow)
  );
  autoNames = filter (n: !(declared ? ${n}) && autoTargets ? ${n}) (attrNames lock);
  autoPin =
    name:
    let
      sourceInfo = fetchPin name;
    in
    if pathExists (sourceInfo.outPath + "/flake.nix") then evalTopFlake sourceInfo { } else sourceInfo;

  self =
    (mapAttrs loadPin declared)
    // listToAttrs (
      map (name: {
        inherit name;
        value = autoPin name;
      }) autoNames
    );
in
self
