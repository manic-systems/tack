# obtaining tack

tack is currently available in nixpkgs, and has its own module.

````nix
{
  programs.tack.enable = true;
}
````

if it's not on your path right now, `nix run nixpkgs#tack` will get you there.

the nixpkgs package tracks tagged releases, but takes some time to update after one lands. if a command here doesn't exist for you yet, or you'd like to pull the latest copy, you can use the repo at `github:manic-systems/tack` instead.
if you plan to do so, [manic.systems](https://manic.systems) provides a binary cache:

````nix
nix.settings = {
  extra-substituters = [ "https://cache.manic.systems" ];
  extra-trusted-public-keys = [
    "cache.manic.systems-1:s6OZanN8Us8vRi0jVivP3qlMn0cYHBjBALKrNe5nH8s="
  ];
};
````

> [!NOTE]
> this repo's nixos module conflicts with upstream, as they both provide `programs.tack`.

if you wish to use the repo module, you should have

````nix
disabledModules = [ "programs/tack.nix" ];
````

in your nix config.

## non-nix

tack is not directly intended to be built outside of nix, but should compile fine for non-nix distros with a plain `cargo build -r`.

dependencies will include a rust toolchain, clang, and wild.

however, as nix itself is expected at runtime for several operations, it should be a fairly obscure situation where this is desired.

## using tack

now that you're ready to pin, let's progress onto your first [tack commands](basic.md).
