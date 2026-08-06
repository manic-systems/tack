# SPDX-License-Identifier: EUPL-1.2
# tack-managed resolver. delete this line to take ownership; tack will leave it alone afterwards.
# tack-resolver: patched tag signedBy

let
  inherit (builtins)
    addErrorContext
    all
    attrNames
    attrValues
    concatMap
    any
    elem
    elemAt
    filter
    foldl'
    fromJSON
    hashFile
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
    split
    stringLength
    substring
    tail
    trace
    ;

  pins = fromTOML (readFile ./pins.toml);
  lock = fromJSON (readFile ./pins.lock.json);
  declared = pins.inputs or { };
  all_follow_raw = pins.all_follow or { };

  validateInputNames =
    { value, location }:
    if !isList value || any (name: !isString name) value then
      throw "tack: ${location} must be an array of strings"
    else
      value;

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

  knownTypes = [
    "github"
    "gitlab"
    "git"
    "tarball"
    "path"
    "indirect"
  ];

  fetchTreeAttrs = {
    type = null;
    owner = null;
    repo = null;
    host = null;
    url = null;
    id = null;
    ref = null;
    rev = null;
    narHash = null;
    lastModified = null;
    revCount = null;
    submodules = null;
    shallow = null;
    allRefs = null;
    name = null;
    lfs = null;
    exportIgnore = null;
    verifyCommit = null;
    keytype = null;
    publicKey = null;
    publicKeys = null;
    dirtyRev = null;
    dirtyShortRev = null;
    unpack = null;
    treeHash = null;
  };

  call =
    {
      overrides ? { },
    }:
    let
      # path nodes are convenience pins, so return the live local path directly
      # because fetchTree rejects unlocked paths in pure eval
      fetchPin =
        name:
        if !(lock ? ${name}) then
          throw "tack: pin '${name}' has no lock entry; run tack update"
        else
          let
            node = lock.${name};
          in
          if (node.type or "") == "path" then
            {
              outPath = if substring 0 1 node.path == "/" then node.path else ./. + ("/" + node.path);
              lastModified = node.lastModified or 0;
            }
            // (if node ? narHash then { inherit (node) narHash; } else { })
          else if !(elem (node.type or "") knownTypes) then
            throw "tack: unknown lock type '${node.type or "?"}' for pin '${name}'"
          else
            fetchTree (intersectAttrs fetchTreeAttrs node);

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

      # tack builds patched trees and adds them to the store, so eval only
      # fetches a locked path and never builds
      fetchPatched =
        { name, pin }:
        let
          node = lock.${name} or { };
          tree =
            node.patched
              or (throw "tack: pin '${name}' has patches but no patched tree, run tack update ${name}");
          vendored =
            digest:
            let
              file = ./. + "/${digest.file}";
            in
            if pathExists file then
              file
            else
              throw "tack: patch ${digest.file} for pin '${name}' is missing, if this is a flake make sure it is tracked by git (git add .tack/patches)";
          current =
            map (digest: digest.source) tree.patches == pin.patches
            && all (digest: hashFile "sha256" (vendored digest) == digest.sha256) tree.patches;
          fetched =
            addErrorContext
              "tack: could not read the patched tree of '${name}', run tack materialize ${name}, or tack update ${name} if the lock was edited by hand"
              (
                fetchTree (
                  {
                    type = "path";
                    inherit (tree) path narHash;
                  }
                  // (if tree ? lastModified then { inherit (tree) lastModified; } else { })
                )
              );
        in
        if !current then
          throw "tack: patches for '${name}' changed since the lock was written, run tack update ${name}"
        else
          fetched
          // (
            if node ? rev then
              {
                dirtyRev = node.rev + "-dirty";
                dirtyShortRev = substring 0 7 node.rev + "-dirty";
              }
            else
              { }
          );

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
        { name, pin }:
        let
          # a rule into this pin's own inputs would make that input follow itself
          prefix = name + "/";
          intoSelf = filter (k: substring 0 (stringLength prefix) all_follow.${k} == prefix) (
            attrNames all_follow
          );
        in
        {
          level = {
            global = removeAttrs all_follow intoSelf;
            local = pin.follows or { };
            excluded = pin.exclude_follow or [ ];
          };
          deep = {
            global = all_follow;
            local = { };
            excluded = pin.exclude_follow or [ ];
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
        { name, pin }:
        let
          localOmitted = validateInputNames {
            value = pin.omit_inputs or [ ];
            location = "inputs.${name}.omit_inputs";
          };
          localKept = validateInputNames {
            value = pin.keep_inputs or [ ];
            location = "inputs.${name}.keep_inputs";
          };
        in
        builtins.deepSeq localOmitted (
          builtins.deepSeq localKept {
            omitted = all_omit_inputs ++ localOmitted;
            kept = localKept;
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

      # `pin/input/...` walks the inputs that pin was evaluated with, as a flake.nix follows does
      resolveFollows = mapAttrs (
        _: target:
        let
          path = filter isString (split "/" target);
          pin = self.${head path} or (throw "tack: follows target '${head path}' is not a pin");
        in
        foldl' (
          node: key:
          (node.inputs or { }).${key} or (throw "tack: follows target '${target}' has no input '${key}'")
        ) pin (tail path)
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

      followsForSide =
        { side, policy }:
        projectFollows {
          inherit side;
          follows = policy.global;
          excluded = policy.excluded;
        }
        // projectFollows {
          inherit side;
          follows = policy.local;
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
          levelFollows,
          deepFollows,
          omit,
        }:
        let
          resolved = resolveFollows levelFollows;
        in
        mapAttrs (
          n: _decl:
          resolved.${n} or (
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
          tackOverrides = resolveFollows (
            intersectAttrs (upPins.inputs or { }) (followsForSide {
              side = "tack";
              policy = levelFollows;
            })
          );
          flakeLevel = intersectAttrs (raw.inputs or { }) (followsForSide {
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
            levelFollows = flakeLevel;
          };

          # upstream declares its outputs forward tackOverrides; a closed `{ self }:`
          # would throw on the extra kwarg, so forward only when declared
          supportsOverrides = (upPins.tack or { }).recomposable or false;

          effectiveTackOverrides = tackOmitOverrides // tackOverrides;
          extraArgs =
            if supportsOverrides && effectiveTackOverrides != { } then
              { tackOverrides = effectiveTackOverrides; }
            else
              { };

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
            if hasTack && effectiveTackOverrides != { } && !supportsOverrides then
              trace "tack: ${flakeDir}: not marked recomposable (set [tack] recomposable = true); overrides will not reach upstream" base
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
          name,
          pin,
          omit,
        }:
        let
          flakeDir = sourceInfo.outPath + (if pin ? dir then "/" + pin.dir else "");
          upLockPath = flakeDir + "/flake.lock";
          upLock = if pathExists upLockPath then fromJSON (readFile upLockPath) else null;
          rootNode = if upLock != null then upLock.root else null;
          f = followsFor { inherit name pin; };
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
          name,
          pin,
          subdir,
          omit,
        }:
        let
          path = sourceInfo.outPath + subdir;
          tackPinsPath = path + "/.tack/pins.toml";
          hasTack = pathExists tackPinsPath;
          upPins = if hasTack then fromTOML (readFile tackPinsPath) else { };
          f = followsFor { inherit name pin; };
          # a fetch drill-in is tack-only
          tackOverrides = resolveFollows (
            intersectAttrs (upPins.inputs or { }) (followsForSide {
              side = "tack";
              policy = f.level;
            })
          );
          supportsOverrides = (upPins.tack or { }).recomposable or false;
          tackOmitOverrides = omitOverridesFor {
            side = "tack";
            inputs = upPins.inputs or { };
            inherit omit;
          };
          effectiveTackOverrides = tackOmitOverrides // tackOverrides;
        in
        # only override tack files within a `fetch`, since there's no flake.lock
        builtins.seq omit (
          if hasTack && effectiveTackOverrides != { } && !supportsOverrides then
            trace "tack: ${path}: not marked recomposable (set [tack] recomposable = true); overrides will not reach upstream" path
          else if hasTack && effectiveTackOverrides != { } then
            let
              upstream = import (path + "/.tack");
            in
            # old resolvers return a plain attrset, not a callable functor
            if upstream ? __functor then
              (upstream { overrides = effectiveTackOverrides; }) // { outPath = path; }
            else
              trace "tack: ${path}: upstream .tack predates override support; overrides will not reach it" path
          else
            path
        );

      loadPin =
        { name, pin }:
        let
          pinType = pin.type or (if pin.flake or true then "flake" else "fetch");
          omit = omitsFor { inherit name pin; };
        in
        builtins.seq omit (
          if pinType == "fixed" then
            fetchFixed {
              inherit name;
              entry = lock.${name};
            }
          else
            let
              sourceInfo =
                if (pin.patches or [ ]) == [ ] then fetchPin name else fetchPatched { inherit name pin; };
              subdir = if pin ? dir then "/" + pin.dir else "";
            in
            if pinType == "flake" then
              evalTopFlake {
                inherit
                  sourceInfo
                  name
                  pin
                  omit
                  ;
              }
            else
              evalFetch {
                inherit
                  sourceInfo
                  name
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
          omit = omitsFor { inherit name pin; };
        in
        if pathExists (sourceInfo.outPath + "/flake.nix") then
          evalTopFlake {
            inherit
              sourceInfo
              name
              pin
              omit
              ;
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
        // overrides;
    in
    builtins.seq all_omit_inputs (self // { __functor = _: call; });
in
call { }
