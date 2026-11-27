# SPDX-License-Identifier: EUPL-1.2

let
  resolver = import ../../.tack;
  inputs = resolver { resolverDir = ./root; };
  replacement = toString inputs.replacement;
  original = toString ./original;
  level2 = toString ./level2;
in
assert
  !(builtins.tryEval (resolver {
    resolverDir = ./bad-global;
  })).success;
assert
  !(builtins.tryEval (resolver {
    resolverDir = ./bad-follow;
  })).success;
assert !(builtins.tryEval inputs.badlocal).success;
assert !(builtins.tryEval inputs.dupfollow).success;
assert !(builtins.tryEval inputs.badexclude).success;
# omitted tack pins point at resolvable lock nodes, so null can only come from
# the omit; a regression resolves them and fails the assert
assert inputs.top.mid.drop == null;
assert (builtins.tryEval inputs.top.mid.keepme).success;
assert (builtins.tryEval inputs.top.mid.scoped).success;
assert toString inputs.top.immediate == replacement;
assert toString inputs.top.mid.dep == replacement;
assert toString inputs.top.mid.immediate != replacement;
assert !(inputs.top ? __tack_policy);
assert inputs.top.policy.omitted == [ "drop" ];
assert inputs.topflake.policy.omitted == [ "drop" ];
# topflake's flake.lock maps `scoped` to a dead node: pure eval cannot fetch a
# real flake input offline, and tryEval cannot catch the dead-node abort, so
# this assert can only pass via the omit firing before node resolution
assert inputs.topflake.inputs.scoped == null;
assert (builtins.tryEval inputs.topflake.nested.scoped).success;
assert inputs.topflake.nested.drop == null;
assert toString inputs.topflake.nested.dep == replacement;
assert
  inputs._meta.replacement == {
    type = "path";
    path = "../replacement";
  };
assert
  inputs._meta.fixedpin == {
    type = "fixed";
    url = "https://example.invalid/fixed.txt";
    sha256 = "0000000000000000000000000000000000000000000000000000";
    unpack = "file";
  };
assert inputs._meta.ghpin.host == "github.com";
assert inputs._meta.ghpin.rev == "0123456789abcdef0123456789abcdef01234567";
assert inputs.top._meta.immediate == inputs._meta.replacement;
assert inputs.top._meta.mid.type == "path";
assert inputs.top.mid._meta.dep == inputs._meta.replacement;
assert !(inputs.top.mid._meta ? drop);
assert inputs.top.mid._meta ? keepme;
assert toString inputs.rechained == replacement;
assert
  inputs._meta.rechained == {
    type = "follows";
    follows = "replacement";
  };
assert !(builtins.tryEval inputs.loopa).success;
# lock-follows/flake.lock locks its nodes by this store path, and copying it
# first lets pure eval fetch them offline. child's `shared` follows
# `parent/dep`, so walking it through parent's outputs would recurse, and
# all_follow overrides that dep
assert builtins.seq (builtins.path {
  path = ./lock-follows/nodes;
  name = "source";
}) (toString inputs.lockflow.parent.child.shared == replacement);
# follows paths walk into tack pins of flake and fetch pins, as those pins'
# resolvers see them with the overrides they're given
assert toString inputs.viaflake == level2;
assert toString inputs.viaflakedeep == replacement;
assert toString inputs.viafetch == replacement;
assert toString inputs.viafetchdeep == replacement;
assert toString inputs.chained == replacement;
assert toString inputs.tablewalk.immediate == level2;
assert
  inputs._meta.chained == {
    type = "follows";
    follows = "topflake/mid/dep";
  };
# topflake has both a flake input and a tack pin named scoped; the flake
# input is omitted, so null can only come from it
assert toString inputs.bareclash == original;
assert toString inputs.tackclash == original;
assert inputs.flakeclash == null;
assert !(builtins.tryEval inputs.notack).success;
assert !(builtins.tryEval inputs.noflake).success;
# an upstream resolver without __tack_nodes still offers its pins, but no deeper
assert toString inputs.vialazy == original;
assert toString inputs.vialegacy == original;
assert !(builtins.tryEval inputs.pastlegacy).success;
assert inputs ? __tack_nodes;
true
