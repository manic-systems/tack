# changelog

notable changes to tack. the format follows
[keep a changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[semver](https://semver.org/).

## [Unreleased]

### upgrading from 1.0.x

- `pins.toml` is read more strictly. a field of the wrong type, like
  `flake = "false"` or `submodules = "true"`, is an error now instead of
  quietly meaning the default. unknown keys at the top level, in an input or
  in a group warn
- a pin name has to be a plain file name, so names with `/` or that are `.` or
  `..` are refused. `_meta`, `_patched` and `_nested` are reserved
- a `rev` in a url has to be a full commit hash. abbreviated ones used to fail
  later at eval, now pins fail to load
- `tack update` and `tack look` fail on a name that matches no input or group,
  where they used to warn and carry on. an unknown `--exclude` name still only
  warns
- before saving a lock, tack checks that `.tack/default.nix` understands the
  features it uses, read from the resolver's `# tack-resolver:` line. a
  resolver that still has its `tack-managed` comment is refreshed in place, so
  expect it to change in your next diff. a forked resolver without the comment
  makes tack refuse to save, so add the features to its `# tack-resolver:` line
  once it handles them, or take tack's again with `tack init --resolver --force`
- overrides only reach the `.tack` of a `fetch` pin's upstream when it sets
  `[tack] recomposable = true`, the same as flake pins. otherwise tack traces
  that it skipped them
- tls is verified against the system ca store instead of a bundled one. where
  there is none, like a bare container, set `SSL_CERT_FILE` to a pem bundle
- the library's errors are `misstep` reports instead of `eyre`, and some
  exports moved or went away, such as `AddRequest`, now `AddArgs`

### Added

- release tags. `tag = "v{version}"` follows the newest matching tag instead of
  a branch. `{version}` may mix `.`, `-` and `_`, a trailing `*` matches a build
  hash after it, named placeholders like `{commit}` capture parts of the tag,
  and ties on version go by commit date. the chosen tag is kept in the lock and
  shown by `look` and `update`
- fixed pins follow release assets by tag, with `{tag}`, `{version}` and named
  placeholders in the url. a github, forgejo, gitea or gitlab release download
  takes the newest tag that already has the asset, and assets hosted anywhere
  else read their tags from `tags_from`, a repo url or an http(s) page
- groups. a `group` key, or nesting as `[inputs.<group>.<pin>]`, prints pins
  under headers in `look` and `update` and lets you name the group to select
  them all. `[groups.<name>]` sets keys for every pin in it
- frozen pins. `tack freeze` and `tack unfreeze` hold pins at the commit they
  have locked, through `update`, until named directly
- `--exclude` on `update`, `look` and `tree`
- `tack tree` prints each pin's locked inputs and follows, including omitted
  inputs and scoped exclusions
- signers. `signers` on a pin requires its commits to be signed by ssh or pgp
  keys you trust, checked across every commit since the last one verified.
  `tack signer add|rm|list` manages them, with `--github` to fetch a user's
  keys, and `tack verify [--base <git-ref>]` checks the lock in ci
- patches. `patches` on a flake or fetch pin applies pull requests, commits,
  diffs or local files in order, without import-from-derivation.
  `tack patch add|update|rm` vendors them, `tack look` reports pull requests
  that merged, closed or changed, and `tack materialize` or
  `nix-build .tack -A _patched` rebuilds patched trees on a machine that
  didn't make them, for your pins and those of tack projects you pin.
  `tack update --refresh-patches` takes newer pull request patches, and a pin
  whose new rev already has a patch drops it
- `[omit_inputs]`, and `omit_inputs` and `keep_inputs` on a pin, keep upstream
  inputs from being fetched or scanned by `dedup`, at every depth and across
  recomposable tack upstreams. evaluating an omitted input warns and gives
  `null`
- follows targets can walk into another pin's inputs, like `hyprland/nixpkgs`
- top-level follows. `[inputs.nixpkgs] follows = "hyprland/nixpkgs"` makes a pin
  of another pin's input, and `tack add <name> --follows <pin/input>` writes one
- the resolver exposes each pin's lock metadata under `_meta`, with `tag`,
  `version` and `group` where they apply
- `fetch_mode = "build"` defers fetching a github or tarball fetch pin to
  build time
- `[tack] type` sets the default pin type
- inputs can be written as inline tables
- shorturls expand recursively, so an alias can point at another alias
- tack finds the enclosing project from a subdirectory
- `update` shows when the old and new revs were committed
- `look` shows commit logs for forgejo and gitea pins, and compares nixpkgs
  channel tarballs by their nixpkgs commit
- shell completions for bash, zsh, fish, nushell and elvish, completing pin and
  group names. the nix package installs them
- `tack rook` is an alias for `tack look`
- examples in `add` and `alias` help
- plain url fetches authenticate from netrc, and git fetches send
  `http.extraHeader` from git config
- the flake advertises the cache.manic.systems binary cache

### Changed

- channel tarballs are locked by their immutable url and dated from it
- the first nix build after `tack update` no longer downloads changed pins
  again. a root eval like `sudo nixos-rebuild` still fetches some once, and
  `TACK_NO_STORE=1` skips the store copies on machines that only update locks
- the resolver falls back to stable fetchers on nix without `fetch-tree`, and
  uses `fetchTarball` for locked tarballs so a cold fetcher cache doesn't
  unpack them again
- `update` relocks pins whose url in `pins.toml` no longer matches the lock,
  and keeps a frozen pin at its rev when only its url changes
- `update` doesn't refetch a fixed pin whose tag hasn't moved, and adds fixed
  downloads to the store so builds don't download them again. they're hashed
  while streaming instead of held in memory
- `dedup` follows the pin graph with omit and follow policy, reads upstream
  lock files with a sparse git fetch, and reports a name that groups disagree
  on instead of picking one
- token hints only show when a request fails for lack of one
- pinned commits are fetched shallowly first, falling back to full history
- commands that write state wait for each other instead of racing
- expected failures print as one line instead of a report
- `--unpack` and `--tags-from` are checked while parsing, and `undo --list`
  takes no other arguments

### Fixed

- `tack --version` reports the real version
- `--exclude` on `update` and `look`
- github tarball downloads authenticate, and gitlab tokens are sent as bearer
  authorization
- `dedup` compares tarball pins by rev instead of sending their url to the
  github api, authenticates its probes, treats an anonymous 404 as a possibly
  private repo, and reports rate-limited or failed probes instead of
  under-reporting duplicates
- new `[all_follow]` targets get locked even when no pin moved, and an unlocked
  one names itself in the error
- a doubled `[all_follow]` alias settles the same way in tack and the resolver
- transitive flake inputs honor `locked.dir`
- tarballs whose locked url has no extension have their format sniffed
- tarball url query params are sorted to match nix's tarball cache
- redirects to an absolute path keep their leading slash
- raw-file probes only run against http(s) git urls, and sparse lock scans skip
  non-http remotes
- plain git hosts are probed once, not on every lookup
- the live pin frame fits the terminal height, and piped `dedup` output has no
  escape codes
- cross builds take clang and the linker from the build platform
- zstd links through pkg-config, as nixpkgs now expects
- an odd gitlab timezone or an alias without a template errors instead of
  panicking, and a panicking worker no longer hangs tack
- `tack init` refuses to overwrite a resolver it can't read

### Security

- git credentials are never sent over cleartext http, and credential hosts are
  derived the way the http client dials them, so a crafted url can't pull
  another host's credential
- netrc values stop at the next keyword, so a stanza missing its login can't
  hand over the next host's credentials
- checkouts create entries exclusively, so a repo with a duplicate name behind
  a symlink can't write outside the tree
- relative submodule urls can't climb past the parent's authority, and remote
  parents can't use local submodule urls
- gitlab commit date lookups encode the rev and bound their offset
- control characters are stripped from commit subjects before printing

## [1.0.1] - 2026-08-03

### Added

- commands run from inside the `.tack` directory
- zstd-compressed tarballs
- `look --verbose` shows ahead/behind counts

### Fixed

- `update` no longer compares local and remote hashes

## [1.0.0] - 2026-06-22

### Added

- the `.tack/` layout for `pins.toml`, `pins.lock.json` and the resolver
- `fixed` pins for hash-locked downloads
- `tack dedup`, with `[all_follow]` and synthesised targets for transitive
  inputs
- history with `tack undo` and `tack redo`
- gitlab, forgejo and gitea pins, plain https tarballs and local `path:` pins
- tack-on-tack overrides through follows
- `look --verbose`, `init --resolver` and `init --convert`
- forge tokens from `GH_TOKEN`, and from nix.conf behind an opt-in

### Changed

- git is fetched with gix instead of libgit2, and tls uses rustls instead of
  openssl
- `update` exits nonzero when a pin fails

## [0.1.1] - 2026-05-28

### Added

- git rev pinning and rev drift detection
- `update --accept`

### Fixed

- overrides propagate fully and `dir` is used properly
- re-running `init` no longer clobbers existing files

## [0.1.0] - 2026-05-28

- initial release

[Unreleased]: https://github.com/manic-systems/tack/compare/v1.0.1...HEAD
[1.0.1]: https://github.com/manic-systems/tack/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/manic-systems/tack/compare/v0.1.1...v1.0.0
[0.1.1]: https://github.com/manic-systems/tack/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/manic-systems/tack/releases/tag/v0.1.0
