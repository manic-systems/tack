# more tack features

## alias

if you have used nix for a while, you are likely familiar with shorturls such as `github:user/repo` for flake inputs.

tack `alias` lets you define your own [shorturls](pins-toml.md#shorturls) in the same style, for any place you pull pins from.

an alias must contain a `{path}` in order to template the given pin into it.

aliases can also be "stacked"; they will continue to resolve until they hit a concrete url to pin.

````
$ tack alias gh 'github:{path}'
alias gh = github:{path}

# now we can use "gh:" to reference any github input

$ tack alias manic 'gh:manic-systems/{path}'
alias manic = gh:manic-systems/{path}

# stack another alias on top of it - let's try pinning through these two aliases

$ tack add tack manic:tack
added tack  NEW -> a992d76 (2026-11-22)
````

## tree

`tree` affords you a view of the current lock state. it prints each of your pins, then the flake inputs of each pin as its `flake.lock` has them, marking the ones that follow something else.

````
$ tack tree
nixpkgs  2026-11-20  ab5c18f  github:nixos/nixpkgs
tack     2026-11-22  a992d76  github:manic-systems/tack
````

if you're interested in deduplicating your inputs, you don't need to do it manually - take a look at the [dedup](#dedup) command below.

## exclude

you may come across a situation where you wish to operate on all your pins *except* 1(..n).

tack `update`, `look`, and `tree` allow an `--exclude` switch, which imbibes a comma separated list of pin or [group](pins-toml.md#groups) names to avoid applying that operation to. you can also repeat the flag instead (`--exclude tack --exclude nixpkgs`).

in our example:

````
# inspect everything except tack

$ tack look --exclude tack
[*] nixpkgs  ab5c18f -> f0b41a9 (ahead)

# or exclude everything, and gaze briefly into the void

$ tack look --exclude tack,nixpkgs
no pins selected
````

## freeze

if you desire a more permanent solution than appending `--exclude` to every other prompt until you inevitably forget, `freeze` has your back.

````
$ tack freeze nixpkgs
froze nixpkgs at ab5c18f

$ tack look
[*] nixpkgs  ab5c18f -> f0b41a9 (ahead)
[✓] tack

$ tack update
[·] nixpkgs  frozen
[✓] tack
````

note that explicitly updating a frozen pin with `tack update <pin name>` WILL update it, and freeze it at the new rev.

you may also `unfreeze` a pin whenever you wish.

## dedup

if you are interested in minimising fetches and store duplication, you can use `dedup` to analyse your locked pins.
`dedup` also gives advice on how to deduplicate your pins.

let's try adding a few more pins to our repo and see what it produces.

````
$ tack add cade manic:cade
added cade  NEW -> ae2aa77 (2026-11-18)

$ tack add nixos-core manic:nixos-core
added nixos-core  NEW -> 3abf365 (2026-11-21)

$ tack dedup
scanning 4 pin(s)...

github:nix-community/fenix  x2
  06f58b8 = fenix  tack
  ca76a73 ↓ fenix  cade

github:rust-lang/rust-analyzer  x2
  946b80a = rust-analyzer-src  tack > fenix
  f09f648 ↓ rust-analyzer-src  cade > fenix

share via [all_follow] in pins.toml:
  # auto-dedup (no top-level pin needed):
  fenix             = "fenix"
  rust-analyzer-src = "rust-analyzer"
````

as you can see, `tack` and `cade` both pull their own copy of `fenix` (and of the `rust-analyzer` source fenix brings with it), and these can be deduplicated using the `all_follow` directive.

if an `all_follow` points at a name with no top-level pin, tack's auto-deduplication will lock it on `tack update`, from the newest copy found among your pins' inputs.

as this is a more complex operation, the advice given is for the [declarative](pins-toml.md) configuration - it's not possible to express `all_follow` in the imperative interface at present.

