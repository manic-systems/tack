// SPDX-License-Identifier: EUPL-1.2

use std::collections::BTreeMap;

use super::{
    PinType,
    PinsDoc,
    Unpack,
};

fn doc(raw: &str) -> PinsDoc {
    PinsDoc::parse(raw).expect("parse")
}

#[test]
fn all_follows_array_form_implies_key_alias() {
    let doc = doc("[all_follow]\ngit-hooks = [\"git-hooks-nix\"]\n");
    let map = doc.all_follows().unwrap();

    assert_eq!(map.get("git-hooks").map(String::as_str), Some("git-hooks"));
    assert_eq!(
        map.get("git-hooks-nix").map(String::as_str),
        Some("git-hooks")
    );
}

#[test]
fn inputs_read_type_unpack_and_legacy_flake_from_each_entry() {
    let doc = doc(r#"
[inputs.default]
url = "github:o/default"

[inputs.source]
url = "github:o/source"
type = "fetch"

[inputs.archive]
url = "https://example.com/archive.tar.gz"
type = "fixed"
unpack = "tarball"

[inputs.legacy]
url = "github:o/legacy"
flake = false
"#);

    let parsed = doc.inputs().expect("inputs");
    let by_name = parsed
        .iter()
        .map(|inp| (inp.name.as_str(), inp))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(by_name["default"].pin_type, PinType::Flake);
    assert_eq!(by_name["source"].pin_type, PinType::Fetch);
    assert_eq!(by_name["archive"].pin_type, PinType::Fixed);
    assert_eq!(by_name["archive"].unpack, Some(Unpack::Tarball));
    assert_eq!(by_name["legacy"].pin_type, PinType::Fetch);
}

#[test]
fn omit_inputs_read_global_and_per_input_sets() {
    let doc = doc(r#"
[omit_inputs]
names = ["flake-compat", "tack:cachix"]

[inputs.foo]
url = "github:o/foo"
omit_inputs = ["nix-test-runner"]
keep_inputs = ["nixpkgs"]
"#);

    let input = doc.inputs().unwrap().pop().unwrap();
    assert!(doc.omit_inputs().unwrap().contains("flake-compat"));
    assert!(input.omit_inputs.contains("nix-test-runner"));
    assert!(input.keep_inputs.contains("nixpkgs"));
}

#[test]
fn omit_inputs_accepts_wildcard_and_scoped_names() {
    let doc = doc("[omit_inputs]\nnames = [\"*\", \"flake:systems\"]\n");
    let omit = doc.omit_inputs().unwrap();
    assert!(omit.contains("*"));
    assert!(omit.contains("flake:systems"));
}
