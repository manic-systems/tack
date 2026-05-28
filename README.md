# tack

flake-like toml nix pins, lazily fetched and transformed

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
outputs = { self }:
  let inputs = import ./.tack; in {
    packages.x86_64-linux.default =
      inputs.nixpkgs.legacyPackages.x86_64-linux.hello;
  };
```

tack keeps `default.nix` in sync with the running binary while a
`tack-managed` comment is present at its top; delete that line to fork
the resolver.

legacy `./inputs.nix` at repo root is detected and preserved as-is.

## commands

```
tack init [--force]
tack update [names...] [--accept]    fetch latest, rewrite lock
tack look [names...]                 report pins with newer upstream revs
tack add <name> <url> [--fetch|--fixed [--unpack tarball|file]]
                      [--dir <d>] [--submodules] [--follows c=p]...
tack rm <name>
tack alias <name> <template>         define a shorturl scheme
tack alias --rm <name>               remove one
tack dedup [--deep]                  report inputs reachable from multiple pins
```

`tack dedup` reports inputs reachable from more than one of your pins —
direct or transitive. when a top-level pin matches, it suggests a
`[all_follow]` rule to share it. `--deep` recurses through the pins of your
pins and so on indefinitely.

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
- `https://...` / `http://...` raw tarball, where the format is inferred
  from the extension (e.g. `.tar`, `.tar.gz`/`.tgz`, `.tar.xz`/`.txz`).

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

`all_follow` applies a rule to every pin that has a matching input

```toml
[all_follow]
nixpkgs = "nixpkgs"   # every input named nixpkgs follows your nixpkgs pin

[inputs.bar]
url = "gh:owner/bar"
exclude_follow = ["nixpkgs"]   # ...except bar's
```

## build

```
nix develop   # rust toolchain + openssl/libgit2
nix build     # the binary
```

## license

EUPL-1.2. see [LICENSE](LICENSE)
