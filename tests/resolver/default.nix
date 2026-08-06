# SPDX-License-Identifier: EUPL-1.2

let
  resolver = import ../../.tack;
  inputs = resolver { resolverDir = ./root; };
  replacement = toString inputs.replacement;
in
assert
  !(builtins.tryEval (resolver {
    resolverDir = ./bad-global;
  })).success;
assert !(builtins.tryEval inputs.badlocal).success;
# omitted tack pins point at resolvable lock nodes, so a caught failure can
# only be the omit throw; a regression resolves them and fails the assert
assert !(builtins.tryEval inputs.top.mid.drop).success;
assert (builtins.tryEval inputs.top.mid.keepme).success;
assert (builtins.tryEval inputs.top.mid.scoped).success;
assert toString inputs.top.immediate == replacement;
assert toString inputs.top.mid.dep == replacement;
assert toString inputs.top.mid.immediate != replacement;
assert !(inputs.top ? __tack_policy);
assert !(inputs.legacy ? __tack_policy);
assert toString inputs.legacy.value == replacement;
assert inputs.__tack_policy_version == 1;
# topflake's flake.lock maps `scoped` to a dead node: pure eval cannot fetch a
# real flake input offline, and tryEval cannot catch the dead-node abort, so
# this assert can only pass via the omit throw firing before node resolution
assert !(builtins.tryEval inputs.topflake.inputs.scoped).success;
assert (builtins.tryEval inputs.topflake.nested.scoped).success;
assert !(builtins.tryEval inputs.topflake.nested.drop).success;
assert toString inputs.topflake.nested.dep == replacement;
true
