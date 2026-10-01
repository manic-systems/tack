# tack

flake-like toml nix pins

maintains `pins.toml` (what you want), `pins.lock.json` (what's fetched),
and a vendored `default.nix` resolver to consume locked inputs without
nix's flake machinery — all tucked into `./.tack/` so your repo root
stays clean.

## layout

`tack init` creates `./.tack/` (override with `$TACK_DIR`) containing:

- `pins.toml` inputs and shorturl schemes, hand-editable
- `pins.lock.json` resolved inputs, written by `tack update`, read by nix
- `default.nix` the resolver; `import ./.tack` gives a name -> input attrset

```nix
let inputs = import ./.tack;
in inputs.nixpkgs.legacyPackages.x86_64-linux.hello
```

or from a flake:

```nix
outputs = { self, ... }@args:
  let inputs = (import ./.tack) {
    overrides = args.tackOverrides or { };
  }; in {
    packages.x86_64-linux.default =
      inputs.nixpkgs.legacyPackages.x86_64-linux.hello;
  };
```

tack warns when `default.nix` has drifted from the running binary, as long as
the `tack-managed` comment at its top is present. run `tack init --resolver` to
update it, or delete that comment to fork the resolver and silence the warning.

the `@args` form lets a parent tack project override this project's
pins through the [follows](#follows) machinery. omit it for a closed
project that doesn't want to be re-composed.

legacy `./inputs.nix` at repo root is detected and preserved as-is.

## commands

```
tack init [--force] [--resolver] [--flake]
                                      scaffold .tack/ (--resolver writes only default.nix,
                                      --flake also a wired flake.nix)
tack update [names|groups...] [--accept] [--exclude <names>]...
                                      fetch latest, rewrite lock
tack look [names|groups...] [--verbose|-v] [--exclude <names>]...
                                      report pins with newer upstream revs
tack tree [names|groups...] [--exclude <names>]...
                                      show each pin's locked inputs and follows
tack verify [--base <git-ref>]       check locked commits against declared signers
tack add <name> <url> [--fetch|--fixed [--unpack tarball|file]]
                      [--dir <d>] [--submodules] [--follows c=p]...
                      [--tag <template>]
tack rm <name>
tack freeze <names|groups...>        hold pins at their locked rev
tack unfreeze <names|groups...>      let update move them again
tack alias <name> <template>         define a shorturl scheme
tack alias --rm <name>               remove one
tack signer add <name> <key-or-file> | --github <user>
                                      trust a signer's keys
tack signer rm <name>                drop a signer no pin lists
tack signer list                     show signers and the pins that list them
tack patch add <name> <source>       apply a patch to a pin
tack patch update [names...]         re-download remote patches
tack patch rm <name> <source>        drop one
tack materialize [names...]          rebuild patched trees missing from the store
tack dedup                           report inputs reachable from multiple pins
```

`tack dedup` reports inputs reachable from more than one of your pins, whether
direct or transitive, and recurses through the pins of your pins indefinitely.
its output is two sub-blocks of ready-to-paste `[all_follow]` rules. the first
block refers to existing top-level pins, the subsequent one refers to inputs tack
will synthesise on the next `tack update`. targets with multiple aliases collapse
into a single array entry.

## pin types

- `flake` (default) — evaluate the input's `flake.nix`, expose its outputs
- `fetch` — source tree only, no flake eval. legacy `flake = false`. follows
  reach into an upstream `.tack` only when it sets `[tack] recomposable = true`
- `fixed` — hash-locked download; won't drift, `tack update` refuses to
  silently relock (use `--accept` if you want to)

```toml
[inputs.release]
url = "https://example.com/release-1.2.3.tar.gz"
type = "fixed"
# unpack = "tarball" | "file"   # auto-detected from the URL
```

## url schemes

- `github:owner/repo[/ref]` tarball via codeload
- `git+https://...` / `git+ssh://...` any git remote; `?ref=<branch>` /
  `?rev=<sha>` to pin, `submodules = true` to recurse
- `path:/absolute/local/tree` or `path:./relative/tree` local convenience pins;
  tack tracks absolute paths with a fast metadata fingerprint instead of
  content-hashing them on every update
- `https://...` / `http://...` raw tarball, where the format is inferred
  from the extension (e.g. `.tar`, `.tar.gz`/`.tgz`, `.tar.xz`/`.txz`,
  `.tar.zst`/`.tzst`).

## shorturls

`scheme:rest` expands by substituting `rest` into the template `{path}`
and repeats until the scheme has no alias. cycles are rejected.

```toml
[shorturls]
gh = "github:{path}"
manic = "gh:manic-systems/{path}"

[inputs.coolproject]
url = "gh:owner/coolproject"

[inputs.tack]
url = "manic:tack"
```

## release tags

the `tag` field follows the newest release tag matching a template instead of a branch

```toml
[inputs.proton-ge]
url = "gh:GloriousEggroll/proton-ge-custom"
type = "fetch"
tag = "GE-Proton{version}"
```

`{version}` matches numbers joined by one separator, either `.`, `-` or `_`,
used consistently, and the highest version wins. `v{version}` takes `v2.1.0` over
`v2.0.9` and `GE-Proton{version}` matches `GE-Proton10-17`, while mixed tags like
`v2.1.0-1`, `v1.0.0-20240101` and `v2.0.3-purple` are skipped. tack lists
tags over the git protocol, so any github, gitlab or network git url works, as
long as it names no ref or rev of its own. local `file://` urls are not
supported. the chosen tag is kept in the lock and shown by `tack look` and `tack update`.

a fixed pin follows the tag's release asset instead, named in its url with
`{tag}` and `{version}`, where `{version}` is the tag from its first digit

```toml
[inputs.proton-ge]
url = "https://github.com/GloriousEggroll/proton-ge-custom/releases/download/{tag}/{tag}.tar.gz"
type = "fixed"
unpack = "tarball"
tag = "GE-Proton{version}"
```

the url must be a GitHub, Forgejo, Gitea or GitLab release download, since its
tags come from that repo. tack takes the newest matching tag whose release
already has the asset, so a release tagged before its uploads finish is
skipped.

## groups

tag pins with a `group` to print them under headers in `tack look` and
`tack update`, and to select them together

```toml
[inputs.niri]
url = "gh:niri-wm/niri"
group = "desktop"
```

`tack update desktop` updates every pin in the group, and
`--exclude desktop` leaves them all alone. names and groups mix freely, so
`tack look desktop --exclude niri` works. a group can't share its name with
a pin.

`tack freeze desktop` sets `frozen = true` on each pin in the group. a frozen
pin stays at its locked rev through `tack update` and `tack update desktop`,
and only moves when named directly, as in `tack update niri`. `tack look` still
reports how far behind it is.

## follows

point a pin's input at one of your top-level pins instead of its own lock

```toml
[inputs.foo]
url = "gh:owner/foo"
follows = { nixpkgs = "nixpkgs" }   # foo's nixpkgs -> your nixpkgs pin
```

a target can also walk into another pin's inputs, like a flake.nix follows path

```toml
[inputs.hypridle]
url = "gh:hyprwm/hypridle"
follows = { hyprlang = "hyprland/hyprlang", nixpkgs = "hyprland/nixpkgs" }
```

`all_follow` applies a rule to every pin that has a matching input. two value
shapes are accepted:

```toml
[all_follow]
# alias -> target. every input named fenix follows your top-level fenix pin
fenix = "fenix"

# target -> [aliases]. the key is the canonical target, and the key plus every
# array member alias to it. one row covers many aliases of the same target
nixpkgs = ["nixpkgs-stable", "nixpkgs-unstable"]

[inputs.bar]
url = "gh:owner/bar"
exclude_follow = ["nixpkgs"]   # ...except bar's
```

when a target named in `[all_follow]` isn't itself a top-level `[inputs]` pin,
`tack update` synthesises a lock entry for it by walking every top-level
flake.lock, collecting the observed revs of the aliased name, and preferring
the branch-ahead rev when GitHub can compare the commits. when comparison is
unavailable or histories have diverged, it falls back to `lastModified`. the
resolver then treats the synthetic entry as a default flake, or as a bare
source tree when its repo has no `flake.nix`. this lets you dedup transitive
inputs (e.g. `crane`) without declaring them as top-level pins you don't
actually consume.

follows reach an upstream's tack pins too, provided the upstream wired
its flake for it (see [publishing](#publishing)). when an upstream has
both a flake input and a tack pin under the same name, a follow on that
name reaches both. scope it with a `flake:` or `tack:` prefix to hit
just one side:

```toml
[inputs.bar]
follows = { "flake:systems" = "systems", "tack:nixpkgs" = "nixpkgs" }
```

`tack tree` prints every pin's flake inputs as the resolver wires them. follows
fold onto one line per pin, so what stands out are the inputs that bring their
own copy, along with any other pins that pull in that same copy, which are the
candidates for an `[all_follow]` rule.

## signers

require a pin's commits to be signed by keys you trust

```toml
[signers]
alice = "ssh-ed25519 AAAA..."
bob = ["keys/bob.keys", "keys/bob.asc"]

[inputs.tool]
url = "gh:alice/tool"
signers = ["alice", "bob"]
```

a signer is an SSH public key line, an ASCII-armored PGP public key, or a path
under `.tack` to a file holding either, and an array gives one signer several
keys. `tack signer add bob --github bob` fetches the signing keys a GitHub user
publishes into `keys/`, `tack signer add alice <key-or-file>` takes one you
already have, and `tack signer rm alice` drops a signer no pin lists anymore,
deleting its files under `keys/` that no other signer uses. `tack signer list`
shows each signer and the pins that list it. add and list both print a
fingerprint per key, to check against the signer's own `ssh-add -L` or `gpg -K`.

the first time `tack update` locks a pin with signers, it checks only the locked
commit, so the history before it is taken on trust, and records who signed it
beside the lock entry, along with a digest of that signer's keys and the first
commit it verified. dropping that signer from the pin starts over the same way,
and so does changing the signer's keys under the same name, with a warning. from
then on that commit is the anchor, and moving the pin requires every commit
after it to be signed, not just the new tip, so one signed commit can't vouch
for unsigned ones before it. a commit that fails, or a new rev that no longer
descends from the anchor, keeps the pin where it was, except that moving back
to a commit between the first verified one and the anchor is accepted, since
the chain already checked it. going back past the first verified commit is
refused. the update line names the signer, and `signed by github` deserves a
second look, since GitHub signs every merge made in its web UI with the key at
`https://github.com/web-flow.gpg`.

checks run the way git does, with `ssh-keygen` for SSH signatures and `gpg` for
PGP ones, so the matching tool must be on `PATH`. github, gitlab, and git pins
can take signers.

signers gate `tack update` only, and the `signedBy` in the lock is plain JSON
that nothing authenticates, so a pull request can hand-edit it. run `tack verify
--base origin/main` as a required CI check. it ignores the lock's `signedBy`,
reads the base lock from git, verifies every commit from the base rev to the
locked one for pins that stay on the same source, and checks the tip alone for
new pins. without `--base` it checks each locked tip. it also reads the base
`pins.toml`, fails a pin whose signers were removed since the base, and notes a
pin removed outright. every `ok` line says when only the tip was checked and
why, and when the trusted signers or their keys changed, as in `ok  signed by
alice (keys changed: alice, tip only, signers added: mallory)`.

## patches

list patches on a flake or fetch pin and tack applies them in order, with no
import-from-derivation

```toml
[inputs.nixpkgs]
url = "gh:NixOS/nixpkgs/nixos-unstable"
patches = [
  "https://github.com/NixOS/nixpkgs/pull/444444",
  "patches/nixpkgs/local-fix.patch",
]
```

a source is a pull request or commit url on GitHub, Forgejo or Gitea, a merge
request or commit url on GitLab, any other https url serving a diff, or a file
relative to `.tack`. sources expand through `[shorturls]` like pin urls, and
`github:` or `gitlab:` paths work too, so with `nixpkgs-pr =
"https://github.com/NixOS/nixpkgs/pull/{path}"` a source can be
`nixpkgs-pr:444444`. `tack patch add nixpkgs <source>` vendors
remote patches into `patches/<pin>/`, and copies local files from outside
`.tack` there too, so every patch you build against is checked in. nothing is
vendored unless the patch applies.

tack applies the patches itself, adds the result to the nix store, and locks
its store path and narHash, so eval only ever reads a path that already exists.
`tack update` reapplies them to each new rev, and a patch that stops applying
keeps the pin on the rev it had. when the patch failed because upstream already
has its change, tack says so and points at `tack patch rm`. `tack update` never
re-downloads a patch, so a force-pushed pull request can't slip in, and
`tack patch update` is how you take the new version on purpose. `tack look`
notes each vendored pull or merge request that has merged, closed, or changed
since you vendored it.

patched trees only exist in the store of the machine that built them. on a
fresh machine or in CI, run `tack materialize` before evaluating. it rebuilds
each tree and refuses one whose hash differs from the lock.

hunks may sit at a different line than the patch says, and hand-edited hunk
counts are recounted the way GNU patch does, but context lines must match
exactly, since its fuzz isn't supported. binary, symlink, and submodule changes
aren't either. when a patch stops applying, the hunks that failed land in a
`.rej` file next to it, a patch of their own to fix up by hand. `tack patch rm`
deletes a remote patch's vendored copy but leaves local files alone, and
`tack undo` restores `patches/` along with the lock.

## laziness

unlike a flake.nix, inputs defined in `tack.toml` will be fetched lazily. if you
have two inputs:

```toml
[inputs.nixpkgs]
url = "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.xz"

# nightly rust for the `fmt` devshell only.
[inputs.fenix]
url = "github:nix-community/fenix"
```

and you don't access the fenix input in your primary code path, it will never be
fetched.

however, flake inputs _of_ your used tack inputs will be fetched eagerly. if you
have an `crate2nix` input used in your primary code path:

```toml
[inputs.nixpkgs]
url = "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.xz"

[inputs.crate2nix]
url = "github:nix-community/crate2nix"
follows = { nixpkgs = "nixpkgs" }
```

then the transitive inputs (`flake-compat`, `devshell`, `nix-test-runner`,
`cachix`, and `pre-commit-hooks`) will be fetched, even though they're never
used.

## publishing

if third parties consume your project as a tack pin, wire your flake so
their [follows](#follows) can reach your pins — thread `tackOverrides`
through `outputs` as in [layout](#layout). `tack init --flake` writes a
wired flake for you; on an existing flake `tack init` prints the
snippet. without the wiring, downstream overrides don't reach your
pins.

## build

```
nix develop   # rust toolchain
nix build     # the binary
```

## run

```
nix run github:manic-systems/tack -- init
nix run github:manic-systems/tack -- add nixpkgs github:NixOS/nixpkgs/nixpkgs-unstable
```

## license

EUPL-1.2. see [LICENSE](LICENSE)
