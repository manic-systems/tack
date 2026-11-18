# tack

tack is a full-featured, user-focused pinning system for nix inputs.

it provides:
- declarative inputs from any nix-compatible source
- easy and convenient management of updates and rollbacks
- mechanisms for following branches, tags, fixed inputs
- automated patch following
- input grouping
- user-defined shorturls (like `myforge:someuser/somerepo`)
- commit signing verification

and much, much more.

to learn more about what tack is and what it offers, check out the [documentation](https://docs.manic.systems/tack).

## completions

The Nix package installs completions for Bash, Zsh, Fish and Nushell. To set
completions up by hand, source the script tack prints.

| shell  | setup                                                    |
|--------|----------------------------------------------------------|
| bash   | `source <(COMPLETE=bash tack)`                           |
| zsh    | `COMPLETE=zsh tack > ~/.zfunc/_tack`, with it in `fpath` |
| fish   | `COMPLETE=fish tack \| source`                           |
| nu     | `COMPLETE=nu tack \| save tack.nu`, then `source` it     |
| elvish | `eval (env COMPLETE=elvish tack \| slurp)`               |

## binary cache

cache.manic.systems offers a substituter that should contain the latest tack.

```nix
nix.settings = {
  extra-substituters = [ "https://cache.manic.systems" ];
  extra-trusted-public-keys = [
    "cache.manic.systems-1:s6OZanN8Us8vRi0jVivP3qlMn0cYHBjBALKrNe5nH8s="
  ];
};
```

## license

EUPL-1.2. see [LICENSE](LICENSE)
