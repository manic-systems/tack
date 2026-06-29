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
tack update [names...] [--accept]    fetch latest, rewrite lock
tack look [names...] [--verbose|-v]  report pins with newer upstream revs
tack add <name> <url> [--fetch|--fixed [--unpack tarball|file]]
                      [--dir <d>] [--submodules] [--follows c=p]...
tack rm <name>
tack alias <name> <template>         define a shorturl scheme
tack alias --rm <name>               remove one
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
- `fetch` — source tree only, no flake eval. legacy `flake = false`
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

```toml
[shorturls]
gh = "github:{path}"

[inputs.coolproject]
url = "gh:owner/coolproject"
```

## follows

point a pin's input at one of your top-level pins instead of its own lock

```toml
[inputs.foo]
url = "gh:owner/foo"
follows = { nixpkgs = "nixpkgs" }   # foo's nixpkgs -> your nixpkgs pin
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
