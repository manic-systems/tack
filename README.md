# tack

flake-like toml nix pins, lazily fetched and transformed

maintains `pins.toml` (what you want) and `pins.lock.json` (what's fetched),
plus an `inputs.nix` resolver to consume locked inputs without nix's flake
machinery

## layout

`tack init` writes three files to the current dir (or `$TACK_DIR`)

- `pins.toml` inputs and shorturl schemes, hand-editable
- `pins.lock.json` resolved inputs, written by `tack update`, read by nix
- `inputs.nix` the resolver; `import ./inputs.nix` gives a name -> input attrset

flake-free config

```nix
let inputs = import ./inputs.nix;
in inputs.nixpkgs.legacyPackages.x86_64-linux.hello
```

or a flake

```nix
outputs = { self }:
  let inputs = import ./inputs.nix; in {
    packages.x86_64-linux.default =
      inputs.nixpkgs.legacyPackages.x86_64-linux.hello;
  };
```

## commands

```
tack init [--force]
tack update [names...]   fetch latest, rewrite lock
tack look [names...]     report pins with newer upstream revs
tack add <name> <url> [--no-flake] [--dir <d>] [--submodules] [--follows c=p]...
tack rm <name>
tack alias <name> <template>   define a shorturl scheme
tack alias --rm <name>         remove one
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
