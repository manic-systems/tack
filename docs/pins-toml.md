# declarative reference (pins.toml)

the file at `.tack/pins.toml` is your declarative interface to tack.

tack commands edit this file, `tack update` reads and generates the lockfiles from it.

## at a glance

````toml
[tack]
type         = "flake"      # default pin type
fetch_mode   = "eval"       # default fetch_mode for fetch pins
recomposable = true         # let projects pinning this one reach these pins

[shorturls]
gh    = "github:{path}"
manic = "gh:manic-systems/{path}"

[all_follow]
nixpkgs = "nixpkgs"
systems = ["flake-utils-systems", "nix-systems"]

[omit_inputs]
names = ["flake-compat", "tack:cachix"]

[signers]
alice = "ssh-ed25519 AAAA..."
bob   = ["keys/bob.keys", "keys/bob.asc"]

[groups.desktop]
follows = { nixpkgs = "nixpkgs" }

[inputs.nixpkgs]
url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz"

[inputs.tool]
url            = "gh:alice/tool"
type           = "flake"
dir            = "nix"
submodules     = true
follows        = { nixpkgs = "nixpkgs", "flake:systems" = "systems" }
exclude_follow = ["systems"]
omit_inputs    = ["pre-commit-hooks"]
keep_inputs    = ["cachix"]
signers        = ["alice", "bob"]
patches        = ["patches/tool/fix.patch"]
tag            = "v{version}"
group          = "desktop"
frozen         = true

[inputs.desktop.niri]
url = "gh:niri-wm/niri"

[inputs.hyprlang]
follows = "hyprland/hyprlang"

[inputs.proton-ge]
url       = "https://github.com/GloriousEggroll/proton-ge-custom/releases/download/{tag}/{tag}.tar.gz"
type      = "fixed"
unpack    = "tarball"
tag       = "GE-Proton{version}"
````

## names

pin names, group names and signer names all have rules.

- a **pin name** has to be a plain file name. no `/`, and not `.` or `..`.
- pin names are **unique** across the whole file, including pins nested inside groups.
- a **group name** and pin name can't collide.
- a **signer name** may only use letters, digits, `-`, `_` and `.`.
- `_meta`, `_patched`, `_nested` and `$declared` are **reserved names** for internal tack usage.

## [tack]

this contains project-wide settings.

| key | type | default | |
|---|---|---|---|
| `type` | `"flake"`, `"fetch"` or `"fixed"` | `"flake"` | the default pin type |
| `fetch_mode` | `"eval"` or `"build"` | `"eval"` | the default fetch mode for `"fetch"` type pins |
| `recomposable` | bool | `false` | whether this pinset is overridable by downstream tack instances |

`tack add` will use the default pin type defined in `pins.toml`.

`recomposable` is about your project being consumed. without it, a tack project that pins yours can still use it - but it can't override or omit anything inside it.

`tack init --flake` sets your project up to be recomposable, as does `tack init` when it finds a recomposably wired flake.nix already present.

please note: non-tack users consuming a tack project **do not** (and, for flake consumers, realistically cannot) have a useful interface that allows them to override your inputs, regardless of recomposable being marked.

## [shorturls]

shorturls are aliases for fetchable urls, similar to `github:` and friends in flakes, but fully user definable.

they are particularly useful for places you pull a lot of inputs from, or inconvenient-to-type prefixes like `git+ssh://git@git.somefor.ge:33`

````toml
[shorturls]
gh    = "github:{path}"
manic = "gh:manic-systems/{path}"
````

as demonstrated above, shorturls can contain one another, and will resolve recursively until they form a full path.

additionally, when a shorturl expands to a `git+..` url with no query param, its final path segment is used as a ref, i.e. `git+https://host/owner/repo/branch` becomes `git+https://host/owner/repo?ref=branch`. a repo nested deeper than `owner/repo` (like a gitlab subgroup) will have its name read as a branch - you can work around this with an explicit `?ref=`.

shorturls apply to `url`, `tags_from` and `patches`.

## [inputs.\<name>]

one table per pin.

the only required key is `url`.

| key | type | for | |
|---|---|---|---|
| [`url`](#url) | string | all | where the pin comes from |
| [`type`](#type) | string | all | `flake`, `fetch` or `fixed` |
| [`flake`](#type) | bool | all | legacy. `false` means `type = "fetch"` |
| [`dir`](#dir) | string | flake, fetch | subdirectory to use as the input's root |
| [`submodules`](#submodules) | bool | flake, fetch | fetch git submodules |
| [`follows`](#follows) | table | flake, fetch | point this pin's inputs at your pins |
| [`exclude_follow`](#exclude_follow) | array | flake, fetch | `all_follow` rules to skip |
| [`omit_inputs`](#omit_inputs-and-keep_inputs) | array | flake, fetch | inputs to drop |
| [`keep_inputs`](#omit_inputs-and-keep_inputs) | array | flake, fetch | omitted inputs to keep anyway |
| [`tag`](#tag) | string | all | follow release tags |
| [`tags_from`](#tags_from) | string | fixed | where to read tags from |
| [`unpack`](#unpack) | string | fixed | `tarball` or `file` |
| [`fetch_mode`](#fetch_mode) | string | fetch | `eval` or `build` |
| [`signers`](#signers) | array | flake, fetch | who must have signed it |
| [`patches`](#patches) | array | flake, fetch | patches to apply |
| [`group`](#group) | string | all | the group it belongs to |
| [`frozen`](#frozen) | bool or string | all | hold it where it is |

on a fetch pin, follows and omits only reach into its `.tack`, and only when that project is [recomposable](pins-toml.md#tack).

### url

| scheme | example | |
|---|---|---|
| `github:` | `github:owner/repo[/ref]` | a tarball from codeload. the ref can contain `/` |
| `gitlab:` | `gitlab:owner/repo[/ref]` | `?host=` for a gitlab other than gitlab.com |
| `git+https://`, `git+ssh://`, `git+http://`, `git+file://` | `git+https://codeberg.org/owner/repo` | any git remote |
| `https://`, `http://` | `https://example.com/src.tar.gz` | a tarball |
| `path:` | `path:./vendor/thing`, `path:/srv/thing` | a local tree (recommended you use `git+file` instead, where possible) |

**queries**

- `?ref=<branch>` picks a branch on github, gitlab and git urls. on github and gitlab it wins over a ref in the path.
- `?rev=<commit>` pins an exact commit. it must be the full hash.
- `?host=<host>` sets a gitlab host.

**tarballs**

a url which is downloaded and unpacked. tarball format is implied by the extensions `.tar`, `.tar.gz`/`.tgz`, `.tar.xz`/`.txz`, `.tar.zst`/`.tzst`.

lockable tarballs (channels) like `https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz` are locked by their resolved redirect url, and `tack look` compares them as commits.

**paths**

`path:` urls are relative to the project (the directory holding `.tack`). tack stores them relative to `.tack` when they're inside the project, and absolute when they aren't.

a nonexistent path is only a warning, not an error.

absolute paths are tracked with a cheap metadata fingerprint rather than hashed on every update.

it's recommended in most cases to use absolute `git+file://` over `path:`, as it benefits from proper locking behaviours and more efficient (less frequent) copies. keep in mind it only sees committed changes.

### type

| type | what you get |
|---|---|
| `flake` | the upstream's `flake.nix` is evaluated by tack, should match typical flake behaviour. default. |
| `fetch` | the raw tree. same as `flake = false` in a flake |
| `fixed` | a single download locked by hash. `tack update` refuses to relock it without the `--accept` flag |

per-pin type definitions override the global `type`.

### dir

a subdirectory of the source to use as the root. for a flake pin, this should be wherever `flake.nix` is.

### submodules

`true` fetches git submodules. github and gitlab pins with `submodules = true` are fetched over git instead of tarballed.

### follows

point this pin's own inputs at your pins (or their inputs) instead of letting it fetch its own copies.

````toml
[inputs.hypridle]
url     = "gh:hyprwm/hypridle"
follows = { nixpkgs = "nixpkgs", hyprlang = "hyprland/hyprlang" }
````

the key is the name of one of this pin's own direct inputs (use [`[all_follow]`](#all_follow) for treewide).

the value is a pin name, optionally followed by `/`-separated inputs to walk into another pin's inputs (similar to flake follows).

values that go through a [top-level follow](#top-level-follows) are walked through to the real pin.

### top-level follows

when `follows` is a string instead of a table, the whole pin is taken from the specified input.

````toml
[inputs.nixpkgs]
follows = "nixos-core/nixpkgs"
````

it has no url (and no lock entry) of its own, no other options available, and can't be inside a [nested group](#groups).

it may follow another top-level follow (but must eventually resolve to a real pin).

in all other respects, it may be referenced in the same fashion as other pins.

### exclude_follow

[`[all_follow]`](#all_follow) rules that this pin ignores. any of the pin's inputs (or their children) with one of these names keeps its own copy.

`*` skips every rule.

when an upstream has both a flake input and a tack pin of the same name, a name scoped with `flake:` or `tack:` only skips the rule for that side.

### omit_inputs and keep_inputs

input names to drop, or keep despite otherwise being dropped.

an omitted input is never fetched. it resolves to `null` (with a warning) when evaled.

````toml
[inputs.crate2nix]
url         = "github:nix-community/crate2nix"
omit_inputs = ["nix-test-runner", "pre-commit-hooks"]
keep_inputs = ["cachix"]
````

`omit_inputs` adds to [`[omit_inputs]`](#omit_inputs) for this pin and its children. `keep_inputs` likewise undoes any omits beneath it.

follows take precedence over an omit.

names may be scoped with `flake:` or `tack:` like in [`exclude_follow`](#exclude_follow), and `*` matches every input.

### tag

follow the newest release tag instead of a branch.

`tag` is a template - define the tag you want with `{version}` standing in for the part that changes.

````toml
[inputs.proton-ge]
url  = "gh:GloriousEggroll/proton-ge-custom"
type = "fetch"
tag  = "GE-Proton{version}"
````

on `tack update`, tack filters tags against the template and locks the one with the highest `{version}`.

#### the tag template

a template is built from these pieces.

| piece | matches |
|---|---|
| plain text, like `v` or `GE-Proton` | exact text |
| `{version}` | numbers joined by `.`, `-` or `_`, like `2`, `2.1.0`, `10-17` or `11.0-20260703` |
| a named placeholder, like `{commit}` or `{build}` | letters and digits, like `0f3e9a1` |
| a trailing `*` | anything at all, to the end of the tag |

every template has exactly one `{version}`. named placeholders and `*` are optional.

#### how a tag is matched

tack reads the template left to right against the tag.

1. exact plain text
2. `{version}` and named placeholders take the longest run they can, then give back one character at a time until the rest of the template matches
3. unless the template ends in `*`, the tag must be completely matched by the template

if any of these fail, tack filters the tag.

some examples

| template | tag | matches? | ranked by |
|---|---|---|---|
| `v{version}` | `v2.1.0` | yes | `2.1.0` |
| `v{version}` | `v2` | yes | `2` |
| `v{version}` | `v2.1.0-rc1` | no, `-rc1` is left over | |
| `v{version}` | `release-2.1.0` | no, it doesn't start with `v` | |
| `GE-Proton{version}` | `GE-Proton10-17` | yes | `10.17` |
| `cachyos-{version}-slr` | `cachyos-11.0-20260703-slr` | yes | `11.0.20260703` |
| `b{version}-*` | `b4012-9f3c2a1` | yes | `4012` |
| `nightly-{version}.{commit}` | `nightly-20261105.0f3e9a1` | yes, `{commit}` is `0f3e9a1` | `20261105` |
| `v{version}*` | `v2.1.0-rc1` | yes | `2.1.0` |

#### flake and fetch pins

the tag pin's url must be a github, gitlab or network git repo (not `file://`). it can't name a ref or rev of its own, since the tag picks the commit.

#### fixed pins

a fixed pin's url is a template for a single file, filled in from the tag tack picks. it can use `{tag}` (the whole tag), `{version}`, and any named placeholders from the template.

````toml
[inputs.app-nightly]
url  = "https://github.com/acme/app/releases/download/{tag}/app-{commit}-linux.tar.gz"
type = "fixed"
tag  = "nightly-{version}.{commit}"
````

without [`tags_from`](#tags_from), the url must be a release download with no placeholders before the `releases` part.

### tags_from

a definition of where a fixed pin gets its tag list.

it requires a `tag` template, and `{tag}` or `{version}` in the url.

````toml
[inputs.linphone]
url       = "https://download.linphone.org/releases/linux/app/Linphone-{version}-x86_64.AppImage"
type      = "fixed"
unpack    = "file"
tag       = "Linphone-{version}-x86_64.AppImage"
tags_from = "https://download.linphone.org/releases/linux/app/"
````

it can be a git repo or webpage. repos are listed the same as regular tag searches, and web pages are searched as plaintext and matched as many times as possible. a directory listing, json file of releases, or changelog are all candidates.

### unpack

fixed pins only. `tarball` extracts the download, `file` keeps it as a single file.

inferred from the url if not provided.

### fetch_mode

determines when a fetch pin is downloaded.

- `eval` (the default) fetches during evaluation.
- `build` fetches it in a fixed-output derivation at build time.

`build` only works for github and tarball pins without patches or a `.tack` of their own. other pins fall back to `eval`.

a default `fetch_mode` can be set under `[tack]`. a pin level `fetch_mode` will override it.

### signers

names from the `[signers]` table whose keys may sign this pin's commits. every name must exist in `[signers]`.

`tack update` refuses to move the pin unless the commits are signed by one of them, and `tack verify` checks the same in CI. only applies to git pins.

### patches

patches applied to the pin, in order. flake and fetch pins only.

- a pull request or commit url on github, forgejo or gitea
- a merge request or commit url on gitlab
- any other `https://` url serving a diff
- a file path relative to `.tack`
- `github:` and `gitlab:` paths, and [shorturls](#shorturls)

remote patches must come over `https`. `tack patch add` vendors them into `patches/<pin>/`.

once a pull request patch lands upstream, `tack update` removes it from the pin's `patches`.

### group

the [group](#groups) this pin belongs to. a pin nested inside a group can't set it.

### frozen

hold the pin where it is.

`true` holds it at whatever is locked, or a full commit hash (40 or 64 hex characters) holds it at that commit.

a frozen pin can nevertheless be updated by explicitly naming it in an `update <pin name>` command.

## groups

a group name works anywhere a pin name does, so `look`, `update`, `tree`, `freeze` and `--exclude` can apply to them.

a pin joins a group either by having it specified with the `group` key:

````toml
[inputs.niri]
url   = "gh:niri-wm/niri"
group = "desktop"
````

or by sitting inside it when it is named:

````toml
[inputs.desktop.niri]
url = "gh:niri-wm/niri"

[inputs.desktop.hyprlock]
url = "gh:hyprwm/hyprlock"
````

a table which holds no url and holds tables which have their own `url`s is considered a group.

### [groups.\<name>]

sets group-level defaults.

````toml
[groups.desktop]
follows        = { nixpkgs = "nixpkgs" }
exclude_follow = ["systems"]
signers        = ["alice"]
````

it takes any input key except `url`, `group` and `frozen`.

patches set at group level aren't removed when they land upstream. `tack update` warns instead, and you remove them by hand.

## [all_follow]

follows applied to every pin, and all their children.

````toml
[all_follow]
# alias = target
# here, every input named `fenix` follows the pin that "fenix" resolves to
fenix = "fenix"

# target = [aliases]
# inputs named nixpkgs, nixpkgs-stable or
# nixpkgs-unstable all follow your nixpkgs pin
nixpkgs = ["nixpkgs-stable", "nixpkgs-unstable"]
````

a string maps the key (an input name) to a target pin.

an array makes the key the target, and maps every member to it.

the target syntax is identical to [`follows`](#follows), including input-type scoping.

a target that isn't a top-level pin of yours is locked by `tack update` from the newest copy found in your pins' own lockfiles. see [dedup](advanced.md#dedup).

opt out with [`exclude_follow`](#exclude_follow) at the pin or group level. a pin level `follows` overrides `all_follow`.

## [omit_inputs]

input names dropped from every pin, and all their children.

````toml
[omit_inputs]
names = ["flake-compat", "tack:cachix"]
````

`names` takes the same scoping and `*` as [`omit_inputs`](#omit_inputs-and-keep_inputs) on a pin. `keep_inputs` at pin level overrides it.

## [signers]

the keys a pin's `signers` key can name.

````toml
[signers]
alice = "ssh-ed25519 AAAA..."
bob   = ["keys/bob.keys", "keys/bob.asc"]
````

you may provide a string or array, with each entry containing a literal key (ssh, pgp) or relative path (from inside `.tack`) to a file holding one.
