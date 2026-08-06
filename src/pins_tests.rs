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
fn follow_tables_reject_bare_and_scoped_keys_for_one_name() {
    let all_follow = doc("[all_follow]\nfoo = \"a\"\n\"flake:foo\" = \"b\"\n")
        .all_follows()
        .unwrap_err();
    assert_eq!(
        all_follow.to_string(),
        "all_follow has both 'foo' and 'flake:foo', keep only one"
    );

    let array_form = doc("[all_follow]\nfoo = [\"tack:foo\"]\n")
        .all_follows()
        .unwrap_err();
    assert!(array_form.to_string().contains("keep only one"));

    let per_pin = doc(
        "[inputs.top]\nurl = \"github:o/top\"\nfollows = { foo = \"a\", \"tack:foo\" = \"b\" }\n",
    )
    .inputs()
    .unwrap_err();
    assert_eq!(
        per_pin.to_string(),
        "inputs.top.follows has both 'foo' and 'tack:foo', keep only one"
    );

    let distinct_sides = doc("[all_follow]\n\"flake:foo\" = \"a\"\n\"tack:foo\" = \"b\"\n");
    assert_eq!(distinct_sides.all_follows().unwrap().len(), 2);
}
