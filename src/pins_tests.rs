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

#[test]
fn page_pins_keep_their_page_and_template() {
    let parsed = doc(r#"
[inputs.linphone]
url = "https://download.linphone.org/releases/linux/app/Linphone-{version}-x86_64.AppImage"
type = "fixed"
tag = "Linphone-{version}-x86_64.AppImage"
tag_page = "https://download.linphone.org/releases/linux/app/"
tag_regex = 'Linphone-\d[^"]*-x86_64\.AppImage'
"#)
    .inputs()
    .expect("inputs");

    let pin = parsed
        .iter()
        .find(|inp| inp.name == "linphone")
        .expect("pin");
    assert_eq!(pin.pin_type, PinType::Fixed);
    assert_eq!(
        pin.tag.as_ref().map(ToString::to_string).as_deref(),
        Some("Linphone-{version}-x86_64.AppImage")
    );
    assert!(pin.tag_page.is_some());
}

#[test]
fn tag_pages_are_checked_against_their_pin() {
    let slot = "https://e/x-{version}.zip";
    let error = |url: &str, fields: &str| {
        PinsDoc::parse(&format!(
            "[inputs.p]\nurl = \"{url}\"\ntype = \"fixed\"\n{fields}"
        ))
        .expect("toml")
        .inputs()
        .expect_err("rejected")
        .to_string()
    };

    assert_eq!(
        error(
            "https://e/x.zip",
            "tag_page = \"https://e/\"\ntag_regex = \"x\"\n"
        ),
        "input 'p': tag_page needs a `tag` template to rank its tags with"
    );
    assert_eq!(
        error(
            "https://e/x.zip",
            "tag = \"{version}\"\ntag_page = \"https://e/\"\ntag_regex = \"x\"\n"
        ),
        "input 'p': a tag_page fills {tag} or {version} in the pin's url"
    );
    assert_eq!(
        error(slot, "tag = \"v{version}\"\ntag_page = \"https://e/\"\n"),
        "input 'p': a tag_page needs a tag_regex to pick its tags with"
    );
    assert_eq!(
        error(slot, "tag = \"v{version}\"\ntag_regex = \"x\"\n"),
        "input 'p': tag_regex needs a tag_page to read"
    );
    assert_eq!(
        error(
            slot,
            "tag = \"{version}\"\ntag_page = \"ftp://e/\"\ntag_regex = \"x\"\n"
        ),
        "input 'p': tag_page must be an http(s) url, got: ftp://e/"
    );
    assert_eq!(
        error(
            slot,
            "tag = \"{version}\"\ntag_page = \"https://e/{tag}\"\ntag_regex = \"x\"\n"
        ),
        "input 'p': tag_page names a page, so it takes no {tag} placeholder"
    );
    assert!(
        error(
            slot,
            "tag = \"{version}\"\ntag_page = \"https://e/\"\ntag_regex = \"(\"\n"
        )
        .contains("tag_regex '(' is not a valid regex")
    );
}

#[test]
fn tag_pages_only_drive_fixed_pins_naming_their_asset() {
    let error = PinsDoc::parse(
        "[inputs.p]\nurl = \"https://e/x.zip\"\ntype = \"fetch\"\ntag = \"v{version}\"\ntag_page = \
         \"https://e/\"\ntag_regex = \"x\"\n",
    )
    .expect("toml")
    .inputs()
    .expect_err("rejected")
    .to_string();
    assert_eq!(
        error,
        "input 'p': tag_page is only valid for type = \"fixed\""
    );
}

#[test]
fn repo_tagged_assets_keep_reading_their_repo() {
    let parsed = doc(r#"
[inputs.app]
url = "https://github.com/o/app/releases/download/v{version}/app-{version}.AppImage"
type = "fixed"
tag = "v{version}"
group = "binary"
"#)
    .inputs()
    .expect("inputs");

    let pin = parsed.iter().find(|inp| inp.name == "app").expect("pin");
    assert_eq!(
        pin.tag.as_ref().map(ToString::to_string).as_deref(),
        Some("v{version}")
    );
    assert!(pin.tag_page.is_none());
}
