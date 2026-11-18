# first commands

## init

first, initialise tack in a repo with `tack init`.

````
$ tack init
initialized tack in /home/bolt/tack-tutorial/.tack
````

this creates a `.tack` folder, containing:

````
  pins.toml       defines your pins
  pins.lock.json  currently locked versions
  default.nix     `import ./.tack` from your config
````

if you want to define a flake, include the `--flake` flag, and tack will add a pre-wired flake that consumes your tack pins.

if you already have a flake defined, you can use `--convert` to automatically convert your flake pins to tack.

if you'd prefer to wire inputs manually, a bare `import ./.tack` is adequate.

## add

next, let's `add` our first pin.

the syntax is `tack add [pin name] [url]`.

````
$ tack add nixpkgs https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz
added nixpkgs  NEW -> nixos-26.11pre1121687 (2026-11-15)
````

## look

now, let's travel to the near future, where nixpkgs has updated, and `look` to see where we stand:

````
$ tack look
[*] nixpkgs  nixos-26.11pre1121687 -> nixos-26.11pre1122823 (ahead)
````

> [!NOTE]
> we could also `tack look nixpkgs` to check only that input.
> if we wanted to see exactly how many commits are between them, we can use `look` with the `-v` flag to get a detailed insight.

let's try:

````
$ tack look -v nixpkgs
[*] nixpkgs  nixos-26.11pre1121687 -> nixos-26.11pre1122823 (ahead)
             1136 ahead
             0490633    python3Packages.pysim: use unversioned cmd2 (#588340)
             18fa2e9    thunderbird-153-unwrapped: 153.6.0esr -> 153.7.0esr (#589319)
             6f042c2    cliproxyapi: 8.0.71 -> 8.0.75 (#589278)
             d778cdb    python3Packages.aioquic_1_2: move to proper package; mitmproxy: use aioquic_1_2 properly (#588368)
             31462d3    rocksdb_{6_23,7_10,8_11,8_3,9_10}: moved to by-name, rocksdb_11_8: init at 11.8.1 (#587443)
             ... 1131 elided
             1a08097    sc-controller: 0.7.2 -> 0.7.6 (#581441)
````

well, `look` at that..

## update

..we need to `update`.

````
$ tack update
[*] nixpkgs  nixos-26.11pre1121687 (2026-11-15) -> nixos-26.11pre1122823 (2026-11-17) (ahead)
````

(again, we can simply `tack update nixpkgs` if we want to be selective).

## undo

uh oh ! our favourite package is marked broken in this rev. let's `undo` and get back to where we were:

````
$ tack undo
  1m ago   update
> 21h ago  add nixpkgs
  21h ago  init
````

the history indicator shows us that tack has walked back one step - we're now in the same state as when we first added nixpkgs (or whatever other previous action we performed).

`undo`ing further will walk us back through the displayed history. we can also `redo` at any time for the opposite operation.

## rm

you know what ? forget nixpkgs unstable, and locking tarballs too. let's replace it with the stable branch on github.

````
$ tack rm nixpkgs
removed nixpkgs

$ tack add nixpkgs github:nixos/nixpkgs/release-26.05
added nixpkgs  NEW -> ab5c18f (2026-11-20)
````

as you can see, tack supports the flake shorturls you're familiar with, and even allows you to define your own.

## conclusion

that's all you need to know for basic imperative pin management - fire up a project with `tack init`, using either the `--flake` flag or manually adding an `import ./.tack` to wherever you need your pins, and we're ready to build.

if you'd prefer to work with the (deeper) declarative interface exposed by `pins.toml`, go ahead to [pin options](pins-toml.md).

to explore some more complex pinning functionality, click through to [advanced features](advanced.md) and we'll talk about `alias`, `freeze`, and more.
