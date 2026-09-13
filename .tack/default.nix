# SPDX-License-Identifier: EUPL-1.2
# tack-managed resolver. delete this line to take ownership; tack will leave it alone afterwards.

let
  inherit (builtins)
    attrNames
    attrValues
    concatMap
    any
    elem
    elemAt
    filter
    fromJSON
    head
    intersectAttrs
    isAttrs
    isList
    isString
    listToAttrs
    mapAttrs
    match
    pathExists
    readFile
    removeAttrs
    substring
    tail
    trace
    ;

  validateInputNames =
    { value, location }:
    if !isList value || any (name: !isString name) value then
      throw "tack: ${location} must be an array of strings"
    else
      value;

  knownTypes = [
    "github"
    "gitlab"
    "git"
    "tarball"
    "path"
    "indirect"
  ];

  call =
    {
      overrides ? { },
      resolverDir ? ./.,
    }:
    let
      pins = fromTOML (readFile (resolverDir + "/pins.toml"));
      lock = fromJSON (readFile (resolverDir + "/pins.lock.json"));
      declared = pins.inputs or { };
      all_follow_raw = pins.all_follow or { };
      # tackOverrides carries this reserved entry across nested resolver boundaries
      inheritedPolicy = overrides.__tack_policy or { };
      inheritedFollows = inheritedPolicy.follows or { };
      inheritedOmit =
        inheritedPolicy.omit or {
          omitted = [ ];
          kept = [ ];
        };
      pinOverrides = removeAttrs overrides [ "__tack_policy" ];

      all_omit_inputs =
        if !(pins ? omit_inputs) then
          [ ]
        else if !isAttrs pins.omit_inputs then
          throw "tack: omit_inputs must be a table with a names array"
        else
          validateInputNames {
            value = pins.omit_inputs.names or [ ];
            location = "omit_inputs.names";
          };

      # flatten `target = [aliases]` rows alongside `alias = "target"` rows
      all_follow = listToAttrs (
        concatMap (
          key:
          let
            val = all_follow_raw.${key};
          in
          if isList val then
            [
              {
                name = key;
                value = key;
              }
            ]
            ++ map (a: {
              name = a;
              value = key;
            }) val
          else if isString val then
            [
              {
                name = key;
                value = val;
              }
            ]
          else
            [ ]
        ) (attrNames all_follow_raw)
      );

      # hashed path nodes use the store, while legacy or impure nodes stay live
      fetchPin =
        name:
        if !(lock ? ${name}) then
          throw "tack: pin '${name}' has no lock entry; run tack update"
        else
          let
            node = lock.${name};
          in
          if
            (node.type or "") == "path" && !((declared.${name} or { }).impure or false) && node ? narHash
          then
            fetchTree (
              node
              // {
                path = if substring 0 1 node.path == "/" then node.path else resolverDir + ("/" + node.path);
              }
            )
          else if (node.type or "") == "path" then
            {
              outPath = if substring 0 1 node.path == "/" then node.path else resolverDir + ("/" + node.path);
              lastModified = node.lastModified or 0;
            }
          else if !(elem (node.type or "") knownTypes) then
            throw "tack: unknown lock type '${node.type or "?"}' for pin '${name}'"
          else
            fetchTree node;

      fetchFixed =
        { name, entry }:
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

      resolveSpec =
        { upLock, spec }:
        if isList spec then
          walkPath {
            inherit upLock;
            nodeName = upLock.root;
            path = spec;
          }
        else
          spec;

      walkPath =
        {
          upLock,
          nodeName,
          path,
        }:
        if path == [ ] then
          nodeName
        else if !(upLock.nodes ? ${nodeName}) then
          throw "tack: follows path dead-end: no node '${nodeName}' in flake.lock"
        else
          let
            key = head path;
            inputs = upLock.nodes.${nodeName}.inputs or { };
          in
          if !(inputs ? ${key}) then
            throw "tack: follows path dead-end: node '${nodeName}' has no input '${key}'"
          else
            walkPath {
              inherit upLock;
              nodeName = resolveSpec {
                inherit upLock;
                spec = inputs.${key};
              };
              path = tail path;
            };

      followsFor =
        { pin }:
        {
          level = {
            global = all_follow;
            local = pin.follows or { };
            excluded = pin.exclude_follow or [ ];
            inherited = inheritedFollows;
          };
          deep = {
            global = all_follow;
            local = { };
            excluded = pin.exclude_follow or [ ];
            inherited = inheritedFollows;
          };
        };

      matchesInput =
        {
          rules,
          side,
          name,
        }:
        any (rule: rule == "*" || rule == name || rule == "${side}:*" || rule == "${side}:${name}") rules;

      omitsFor =
        { pinName, pin }:
        let
          localOmitted = validateInputNames {
            value = pin.omit_inputs or [ ];
            location = "inputs.${pinName}.omit_inputs";
          };
          localKept = validateInputNames {
            value = pin.keep_inputs or [ ];
            location = "inputs.${pinName}.keep_inputs";
          };
        in
        builtins.deepSeq localOmitted (
          builtins.deepSeq localKept {
            omitted = (inheritedOmit.omitted or [ ]) ++ all_omit_inputs ++ localOmitted;
            kept = (inheritedOmit.kept or [ ]) ++ localKept;
          }
        );

      shouldOmit =
        {
          omit,
          side,
          name,
        }:
        matchesInput {
          rules = omit.omitted;
          inherit side name;
        }
        && !(matchesInput {
          rules = omit.kept;
          inherit side name;
        });

      omittedInput =
        { side, name }:
        throw "tack: ${side} input '${name}' was omitted by omit_inputs";

      resolveFollows = mapAttrs (
        _: target: self.${target} or (throw "tack: follows target '${target}' is not a pin")
      );

      # follow keys are `flake:name`, `tack:name`, or bare `name`
      # project onto one side, rekeyed to bare names; exclusions apply only
      # to global follows, before level-local follows are merged over them
      projectFollows =
        {
          side,
          follows,
          excluded ? [ ],
        }:
        listToAttrs (
          concatMap (
            key:
            let
              m = match "(flake|tack):(.*)" key;
            in
            if
              m == null
              && !(matchesInput {
                rules = excluded;
                inherit side;
                name = key;
              })
            then
              [
                {
                  name = key;
                  value = follows.${key};
                }
              ]
            else if
              m != null
              && head m == side
              && !(matchesInput {
                rules = excluded;
                inherit side;
                name = elemAt m 1;
              })
            then
              [
                {
                  name = elemAt m 1;
                  value = follows.${key};
                }
              ]
            else
              [ ]
          ) (attrNames follows)
        );

      followOverridesForSide =
        { side, policy }:
        resolveFollows (projectFollows {
          inherit side;
          follows = policy.global;
          excluded = policy.excluded;
        })
        // projectFollows {
          inherit side;
          follows = policy.inherited;
        }
        // resolveFollows (projectFollows {
          inherit side;
          follows = policy.local;
        });

      scopedFollowValues =
        side: values:
        listToAttrs (
          map (name: {
            name = "${side}:${name}";
            value = values.${name};
          }) (attrNames values)
        );

      propagatedPolicy =
        { omit, follows }:
        let
          followValues =
            scopedFollowValues "flake" (followOverridesForSide {
              side = "flake";
              policy = follows;
            })
            // scopedFollowValues "tack" (followOverridesForSide {
              side = "tack";
              policy = follows;
            });
          active = (omit.omitted or [ ]) != [ ] || (omit.kept or [ ]) != [ ] || attrNames followValues != [ ];
        in
        {
          inherit active;
          override = {
            __tack_policy = {
              inherit omit;
              follows = followValues;
            };
          };
        };

      omitOverridesFor =
        {
          side,
          inputs,
          omit,
        }:
        listToAttrs (
          map (name: {
            inherit name;
            value = omittedInput { inherit side name; };
          }) (filter (name: shouldOmit { inherit omit side name; }) (attrNames inputs))
        );

      mkCallerInputs =
        {
          upLock,
          nodeName,
          rawInputs,
          levelOverrides,
          deepFollows,
          omit,
        }:
        mapAttrs (
          n: _decl:
          levelOverrides.${n} or (
            if
              shouldOmit {
                inherit omit;
                side = "flake";
                name = n;
              }
            then
              omittedInput {
                side = "flake";
                name = n;
              }
            else if upLock != null then
              let
                ref =
                  (upLock.nodes.${nodeName}.inputs or { }).${n}
                    or (throw "tack: input '${n}' declared but not in flake.lock node '${nodeName}'");
                childName = resolveSpec {
                  inherit upLock;
                  spec = ref;
                };
                childNode = upLock.nodes.${childName};
                childSrc = fetchTree childNode.locked;
              in
              if childNode.flake or true then
                evalTransitive {
                  inherit upLock;
                  nodeName = childName;
                  sourceInfo = childSrc;
                  follows = deepFollows;
                  inherit omit;
                }
              else
                childSrc
            else
              throw "tack: no flake.lock; cannot resolve input '${n}'"
          )
        ) rawInputs;

      mkFlakeResult =
        {
          sourceInfo,
          flakeDir,
          callerInputs,
          outputs,
        }:
        outputs
        // sourceInfo
        // {
          outPath = flakeDir;
          inputs = callerInputs;
          inherit outputs sourceInfo;
          _type = "flake";
        };

      evalFlake =
        {
          sourceInfo,
          flakeDir,
          upLock,
          nodeName,
          levelFollows,
          deepFollows,
          omit,
        }:
        let
          raw = import (flakeDir + "/flake.nix");

          tackPinsPath = flakeDir + "/.tack/pins.toml";
          hasTack = pathExists tackPinsPath;
          upPins = if hasTack then fromTOML (readFile tackPinsPath) else { };

          # project follows onto each side, keep only names that side has
          # bare follow reaches both; `flake:`/`tack:` reaches just one
          tackOverrides = intersectAttrs (upPins.inputs or { }) (followOverridesForSide {
            side = "tack";
            policy = levelFollows;
          });
          flakeLevel = intersectAttrs (raw.inputs or { }) (followOverridesForSide {
            side = "flake";
            policy = levelFollows;
          });
          tackOmitOverrides = omitOverridesFor {
            side = "tack";
            inputs = upPins.inputs or { };
            inherit omit;
          };

          # deep follows pass down raw, so each descendant re-projects per side
          callerInputs = mkCallerInputs {
            inherit
              upLock
              nodeName
              deepFollows
              omit
              ;
            rawInputs = raw.inputs or { };
            levelOverrides = flakeLevel;
          };

          # upstream declares its outputs forward tackOverrides; a closed `{ self }:`
          # would throw on the extra kwarg, so forward only when declared
          supportsOverrides = (upPins.tack or { }).recomposable or false;
          tackResolver = if hasTack then import (flakeDir + "/.tack") else { };
          supportsPolicy = (tackResolver.__tack_policy_version or 0) >= 1;

          effectiveTackOverrides = tackOmitOverrides // tackOverrides;
          propagated = propagatedPolicy {
            inherit omit;
            follows = deepFollows;
          };
          tackCallOverrides = effectiveTackOverrides // (if supportsPolicy then propagated.override else { });
          hasTackOverrides = effectiveTackOverrides != { };
          hasTackPolicy = propagated.active;
          needsTackCall = hasTackOverrides || (hasTackPolicy && supportsPolicy);
          extraArgs =
            if supportsOverrides && needsTackCall then { tackOverrides = tackCallOverrides; } else { };

          outputs = raw.outputs (callerInputs // extraArgs // { self = result; });

          result =
            let
              base = mkFlakeResult {
                inherit
                  sourceInfo
                  flakeDir
                  callerInputs
                  outputs
                  ;
              };
            in
            if hasTack && (hasTackOverrides || hasTackPolicy) && !supportsOverrides then
              trace "tack: ${flakeDir}: not marked recomposable (set [tack] recomposable = true); overrides will not reach upstream" base
            else if hasTack && hasTackPolicy && !supportsPolicy then
              trace "tack: ${flakeDir}: upstream .tack predates recursive policy support; recursive omit/follow rules will not reach it" base
            else
              base;
        in
        result;

      evalTransitive =
        {
          upLock,
          nodeName,
          sourceInfo,
          follows,
          omit,
        }:
        evalFlake {
          inherit
            upLock
            nodeName
            sourceInfo
            omit
            ;
          flakeDir = sourceInfo.outPath;
          levelFollows = follows;
          deepFollows = follows;
        };

      evalTopFlake =
        {
          sourceInfo,
          pin,
          omit,
        }:
        let
          flakeDir = sourceInfo.outPath + (if pin ? dir then "/" + pin.dir else "");
          upLockPath = flakeDir + "/flake.lock";
          upLock = if pathExists upLockPath then fromJSON (readFile upLockPath) else null;
          rootNode = if upLock != null then upLock.root else null;
          f = followsFor { inherit pin; };
        in
        builtins.seq omit (evalFlake {
          inherit
            sourceInfo
            flakeDir
            upLock
            omit
            ;
          nodeName = rootNode;
          levelFollows = f.level;
          deepFollows = f.deep;
        });

      evalFetch =
        {
          sourceInfo,
          pin,
          subdir,
          omit,
        }:
        let
          path = sourceInfo.outPath + subdir;
          tackPinsPath = path + "/.tack/pins.toml";
          hasTack = pathExists tackPinsPath;
          upPins = if hasTack then fromTOML (readFile tackPinsPath) else { };
          f = followsFor { inherit pin; };
          # a fetch drill-in is tack-only
          tackOverrides = intersectAttrs (upPins.inputs or { }) (followOverridesForSide {
            side = "tack";
            policy = f.level;
          });
          tackOmitOverrides = omitOverridesFor {
            side = "tack";
            inputs = upPins.inputs or { };
            inherit omit;
          };
          effectiveTackOverrides = tackOmitOverrides // tackOverrides;
          propagated = propagatedPolicy {
            inherit omit;
            follows = f.deep;
          };
          upstream = if hasTack then import (path + "/.tack") else { };
          supportsPolicy = (upstream.__tack_policy_version or 0) >= 1;
          tackCallOverrides = effectiveTackOverrides // (if supportsPolicy then propagated.override else { });
          hasTackOverrides = effectiveTackOverrides != { };
          hasTackPolicy = propagated.active;
          needsTackCall = hasTackOverrides || (hasTackPolicy && supportsPolicy);
        in
        # a fetch pin is a source tree (path)
        # hand back resolved inputs when overrides or recursive policy reach the upstream's .tack
        builtins.seq omit (
          if hasTack && needsTackCall then
            # old resolvers return a plain attrset, not a callable functor
            if upstream ? __functor then
              let
                resolved = (upstream { overrides = tackCallOverrides; }) // {
                  outPath = path;
                };
              in
              if hasTackPolicy && !supportsPolicy then
                trace "tack: ${path}: upstream .tack predates recursive policy support; recursive omit/follow rules will not reach it" resolved
              else
                resolved
            else
              trace "tack: ${path}: upstream .tack predates override support; overrides will not reach it" path
          else if hasTack && hasTackPolicy && !supportsPolicy then
            trace "tack: ${path}: upstream .tack predates recursive policy support; recursive omit/follow rules will not reach it" path
          else
            path
        );

      loadPin =
        { name, pin }:
        let
          pinType = pin.type or (if pin.flake or true then "flake" else "fetch");
          omit = omitsFor {
            pinName = name;
            inherit pin;
          };
        in
        builtins.seq omit (
          if pinType == "fixed" then
            fetchFixed {
              inherit name;
              entry = lock.${name};
            }
          else
            let
              sourceInfo = fetchPin name;
              subdir = if pin ? dir then "/" + pin.dir else "";
            in
            if pinType == "flake" then
              evalTopFlake { inherit sourceInfo pin omit; }
            else
              evalFetch {
                inherit
                  sourceInfo
                  pin
                  subdir
                  omit
                  ;
              }
        );

      # undeclared lock entries are synthesised into toplevels by auto-dedup
      # only when referenced as [all_follow] targets
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
          pin = { };
          omit = omitsFor {
            pinName = name;
            inherit pin;
          };
        in
        if pathExists (sourceInfo.outPath + "/flake.nix") then
          evalTopFlake {
            inherit sourceInfo pin omit;
          }
        else
          sourceInfo;

      self =
        (mapAttrs (name: pin: loadPin { inherit name pin; }) declared)
        // listToAttrs (
          map (name: {
            inherit name;
            value = autoPin name;
          }) autoNames
        )
        // pinOverrides;
    in
    builtins.seq all_omit_inputs (
      self
      // {
        __functor = _: call;
        __tack_policy_version = 1;
      }
    );
in
call { }
