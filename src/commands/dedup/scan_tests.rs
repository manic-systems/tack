// SPDX-License-Identifier: EUPL-1.2

use super::{
    FollowPolicy,
    InputDecision,
    OmitPolicy,
    ScanDocuments,
    decide_input,
};
use crate::pins::{
    Input,
    PinType,
    PinsDoc,
    Side,
};

fn input(raw: &str) -> Input {
    PinsDoc::parse(raw)
        .unwrap()
        .inputs()
        .unwrap()
        .pop()
        .unwrap()
}

fn policies(
    raw: &str,
    omitted_names: &[&str],
    all_follow_entries: &[(&str, &str)],
) -> (OmitPolicy, FollowPolicy) {
    let input = input(raw);
    let omitted = omitted_names
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let all_follow = all_follow_entries
        .iter()
        .map(|&(name, target)| (name.to_owned(), target.to_owned()))
        .collect();
    (
        OmitPolicy::for_input(&omitted, &input),
        FollowPolicy::for_input(&all_follow, &input),
    )
}

fn scan_flake(raw: &str, omit: &OmitPolicy, follow: &FollowPolicy) -> super::ScanResult {
    ScanDocuments {
        flake_lock: Some(raw.to_owned()),
        tack_pins:  None,
        tack_lock:  None,
    }
    .scan(&["top".to_owned()], omit, follow, PinType::Flake)
}

#[test]
fn omitted_edge_removes_exclusive_descendants() {
    let (omit, follow) = policies("[inputs.top]\nurl = \"github:o/top\"\n", &["drop"], &[]);
    let result = scan_flake(
        r#"{
            "root":"root",
            "nodes":{
                "root":{"inputs":{"drop":"drop"}},
                "drop":{"inputs":{"leaf":"leaf"},"locked":{"type":"github","owner":"o","repo":"drop","rev":"1"}},
                "leaf":{"locked":{"type":"github","owner":"o","repo":"leaf","rev":"2"}}
            }
        }"#,
        &omit,
        &follow,
    );

    assert!(result.findings.is_empty());
}

#[test]
fn shared_descendant_survives_retained_route() {
    let (omit, follow) = policies("[inputs.top]\nurl = \"github:o/top\"\n", &["drop"], &[]);
    let result = scan_flake(
        r#"{
            "root":"root",
            "nodes":{
                "root":{"inputs":{"drop":"drop","keep":"keep"}},
                "drop":{"inputs":{"shared":"shared"},"locked":{"type":"github","owner":"o","repo":"drop","rev":"1"}},
                "keep":{"inputs":{"shared":"shared"},"locked":{"type":"github","owner":"o","repo":"keep","rev":"2"}},
                "shared":{"locked":{"type":"github","owner":"o","repo":"shared","rev":"3"}}
            }
        }"#,
        &omit,
        &follow,
    );

    assert_eq!(
        result
            .findings
            .iter()
            .map(|finding| (finding.entry.path.clone(), finding.entry.name.clone()))
            .collect::<Vec<_>>(),
        vec![
            (vec!["top".to_owned()], "keep".to_owned()),
            (
                vec!["top".to_owned(), "keep".to_owned()],
                "shared".to_owned()
            ),
        ]
    );
}

#[test]
fn configured_follow_beats_omit_and_skips_original_subtree() {
    let (omit, follow) = policies(
        "[inputs.top]\nurl = \"github:o/top\"\nomit_inputs = [\"old\"]\n[inputs.top.follows]\nold \
         = \"replacement\"\n",
        &[],
        &[],
    );
    let result = scan_flake(
        r#"{
            "root":"root",
            "nodes":{
                "root":{"inputs":{"old":"old"}},
                "old":{"inputs":{"leaf":"leaf"},"locked":{"type":"github","owner":"o","repo":"old","rev":"1"}},
                "leaf":{"locked":{"type":"github","owner":"o","repo":"leaf","rev":"2"}}
            }
        }"#,
        &omit,
        &follow,
    );

    assert!(result.findings.is_empty());
    assert_eq!(result.followed.len(), 1);
    assert_eq!(result.followed[0].target, "replacement");
    assert_eq!(result.followed[0].path, ["top"]);
    assert_eq!(result.followed[0].name, "old");
    assert!(result.followed[0].side == Side::Flake);
}

#[test]
fn per_pin_follow_is_level_local() {
    let (omit, follow) = policies(
        "[inputs.top]\nurl = \"github:o/top\"\n[inputs.top.follows]\nfoo = \"replacement\"\n",
        &[],
        &[],
    );
    let result = scan_flake(
        r#"{
            "root":"root",
            "nodes":{
                "root":{"inputs":{"carrier":"carrier","foo":"root-foo"}},
                "carrier":{"inputs":{"foo":"deep-foo"},"locked":{"type":"github","owner":"o","repo":"carrier","rev":"1"}},
                "root-foo":{"locked":{"type":"github","owner":"o","repo":"root-foo","rev":"2"}},
                "deep-foo":{"locked":{"type":"github","owner":"o","repo":"deep-foo","rev":"3"}}
            }
        }"#,
        &omit,
        &follow,
    );

    assert_eq!(result.followed.len(), 1);
    assert_eq!(result.followed[0].path, ["top"]);
    assert_eq!(result.followed[0].name, "foo");
    assert!(result.findings.iter().any(|finding| {
        finding.entry.name == "foo" && finding.entry.path == ["top", "carrier"]
    }));
}

#[test]
fn inherited_follow_is_immune_to_nested_exclusion() {
    let (omit, follow) = policies("[inputs.top]\nurl = \"github:o/top\"\n", &[], &[(
        "foo",
        "replacement",
    )]);
    let result = ScanDocuments {
        flake_lock: None,
        tack_pins:  Some(
            "[tack]\nrecomposable = true\n[inputs.child]\nurl = \
             \"github:o/child\"\nexclude_follow = [\"foo\"]\n"
                .to_owned(),
        ),
        tack_lock:  None,
    }
    .scan(&["top".to_owned()], &omit, &follow, PinType::Fetch);

    let child = &result.transitive[0];
    assert!(matches!(
        decide_input(&child.omit, &child.follow, Side::Flake, "foo", false),
        InputDecision::Follow("replacement")
    ));
}

#[test]
fn unwired_pin_blocks_inherited_policy() {
    let (omit, follow) = policies("[inputs.top]\nurl = \"github:o/top\"\n", &["drop"], &[]);
    let docs = ScanDocuments {
        flake_lock: None,
        tack_pins:  Some("[inputs.drop]\nurl = \"github:o/drop\"\n".to_owned()),
        tack_lock:  None,
    };

    for pin_type in [PinType::Flake, PinType::Fetch] {
        let unwired = docs.scan(&["top".to_owned()], &omit, &follow, pin_type);
        assert_eq!(unwired.findings.len(), 1, "{pin_type}");
    }
}

#[test]
fn consumer_keep_undoes_nested_omit() {
    let (omit, follow) = policies(
        "[inputs.top]\nurl = \"github:o/top\"\nkeep_inputs = [\"drop\"]\n",
        &[],
        &[],
    );
    let result = ScanDocuments {
        flake_lock: None,
        tack_pins:  Some(
            "[tack]\nrecomposable = true\n[omit_inputs]\nnames = [\"drop\"]\n[inputs.child]\nurl \
             = \"github:o/child\"\n"
                .to_owned(),
        ),
        tack_lock:  None,
    }
    .scan(&["top".to_owned()], &omit, &follow, PinType::Fetch);

    let child = &result.transitive[0];
    assert!(matches!(
        decide_input(&child.omit, &child.follow, Side::Flake, "drop", true),
        InputDecision::Traverse
    ));
}

#[test]
fn nested_keep_cannot_undo_consumer_omit() {
    let (omit, follow) = policies("[inputs.top]\nurl = \"github:o/top\"\n", &["dep"], &[]);
    let result = ScanDocuments {
        flake_lock: None,
        tack_pins:  Some(
            "[tack]\nrecomposable = true\n[inputs.child]\nurl = \"github:o/child\"\nkeep_inputs = \
             [\"*\"]\n"
                .to_owned(),
        ),
        tack_lock:  None,
    }
    .scan(&["top".to_owned()], &omit, &follow, PinType::Fetch);

    let child = &result.transitive[0];
    assert!(matches!(
        decide_input(&child.omit, &child.follow, Side::Flake, "dep", true),
        InputDecision::Omit
    ));
}
