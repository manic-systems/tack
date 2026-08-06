// SPDX-License-Identifier: EUPL-1.2

use std::collections::{
    BTreeMap,
    BTreeSet,
};

use super::{
    FollowPolicy,
    InputDecision,
    OmitPolicy,
    ScanDocuments,
    decide_input,
};
use crate::{
    commands::dedup::model::Side,
    pins::{
        FollowSide,
        Input,
        PinType,
        PinsDoc,
        global_follow_target,
    },
    scan_diagnostic::ScanFile,
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
fn scan_records_reachable_gitlab_node() {
    let result = scan_flake(
        r#"{
            "root": "root",
            "nodes": {
                "root": { "inputs": { "dep": "dep" } },
                "dep": { "locked": {
                    "type": "gitlab",
                    "host": "GitLab.Example.Com:8443",
                    "owner": "Group/Sub",
                    "repo": "Repo",
                    "rev": "abc123",
                    "lastModified": 1700
                } }
            }
        }"#,
        &OmitPolicy::default(),
        &FollowPolicy::default(),
    );

    assert_eq!(result.findings.len(), 1);
    let finding = &result.findings[0];
    assert_eq!(finding.entry.name, "dep");
    assert_eq!(finding.entry.path, ["top"]);
    assert_eq!(
        finding.identity.to_string(),
        "gitlab:gitlab.example.com:8443/group/sub/repo"
    );
    assert_eq!(finding.entry.rev, "abc123");
    assert_eq!(finding.entry.lm, Some(1_700));
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
fn shared_node_is_preserved_at_distinct_graph_paths() {
    let result = scan_flake(
        r#"{
            "root":"root",
            "nodes":{
                "root":{"inputs":{"left":"left","right":"right"}},
                "left":{"inputs":{"shared":"shared"},"locked":{"type":"github","owner":"o","repo":"left","rev":"1"}},
                "right":{"inputs":{"shared":"shared"},"locked":{"type":"github","owner":"o","repo":"right","rev":"2"}},
                "shared":{"locked":{"type":"github","owner":"o","repo":"shared","rev":"3"}}
            }
        }"#,
        &OmitPolicy::default(),
        &FollowPolicy::default(),
    );

    let shared = result
        .findings
        .iter()
        .filter(|finding| finding.entry.name == "shared")
        .map(|finding| finding.entry.path.clone())
        .collect::<Vec<_>>();
    assert_eq!(shared, vec![
        vec!["top".to_owned(), "left".to_owned()],
        vec!["top".to_owned(), "right".to_owned()],
    ]);
}

#[test]
fn direct_and_follows_path_refs_resolve() {
    let result = scan_flake(
        r#"{
            "root":"root",
            "nodes":{
                "root":{"inputs":{"alias":["base"],"base":"dep"}},
                "dep":{"locked":{"type":"github","owner":"o","repo":"dep","rev":"1"}}
            }
        }"#,
        &OmitPolicy::default(),
        &FollowPolicy::default(),
    );

    assert_eq!(
        result
            .findings
            .iter()
            .map(|finding| finding.entry.name.as_str())
            .collect::<Vec<_>>(),
        ["alias", "base"]
    );
}

#[test]
fn graph_cycles_terminate_but_record_incoming_occurrence() {
    let result = scan_flake(
        r#"{
            "root":"root",
            "nodes":{
                "root":{"inputs":{"a":"a"}},
                "a":{"inputs":{"b":"b"},"locked":{"type":"github","owner":"o","repo":"a","rev":"1"}},
                "b":{"inputs":{"a":"a"},"locked":{"type":"github","owner":"o","repo":"b","rev":"2"}}
            }
        }"#,
        &OmitPolicy::default(),
        &FollowPolicy::default(),
    );

    assert_eq!(
        result
            .findings
            .iter()
            .map(|finding| finding.entry.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "a"]
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
fn all_follow_applies_deep() {
    let (omit, follow) = policies("[inputs.top]\nurl = \"github:o/top\"\n", &[], &[(
        "foo",
        "replacement",
    )]);
    let result = scan_flake(
        r#"{
            "root":"root",
            "nodes":{
                "root":{"inputs":{"carrier":"carrier"}},
                "carrier":{"inputs":{"foo":"deep-foo"},"locked":{"type":"github","owner":"o","repo":"carrier","rev":"1"}},
                "deep-foo":{"locked":{"type":"github","owner":"o","repo":"deep-foo","rev":"2"}}
            }
        }"#,
        &omit,
        &follow,
    );

    assert_eq!(result.followed.len(), 1);
    assert_eq!(result.followed[0].path, ["top", "carrier"]);
    assert_eq!(result.followed[0].name, "foo");
    assert_eq!(result.followed[0].target, "replacement");
}

#[test]
fn queued_tack_descendants_drop_level_follows() {
    let (omit, follow) = policies(
        "[inputs.top]\nurl = \"github:o/top\"\n[inputs.top.follows]\nfoo = \"replacement\"\n",
        &[],
        &[],
    );
    let result = ScanDocuments {
        flake_lock: None,
        tack_pins:  Some("[inputs.child]\nurl = \"github:o/child\"\n".to_owned()),
        tack_lock:  None,
    }
    .scan(&["top".to_owned()], &omit, &follow, PinType::Fetch);

    assert_eq!(result.transitive.len(), 1);
    assert!(matches!(
        decide_input(
            &result.transitive[0].omit,
            &result.transitive[0].follow,
            Side::Tack,
            "foo",
            true,
        ),
        InputDecision::Traverse
    ));
}

#[test]
fn nested_doc_tables_apply_to_queued_descendants() {
    let (omit, follow) = policies("[inputs.top]\nurl = \"github:o/top\"\n", &["drop"], &[]);
    let result = ScanDocuments {
        flake_lock: None,
        tack_pins:  Some(
            "[omit_inputs]\nnames = [\"doc-drop\"]\n[inputs.child]\nurl = \
             \"github:o/child\"\nomit_inputs = [\"local-drop\"]\nkeep_inputs = [\"drop\"]\n"
                .to_owned(),
        ),
        tack_lock:  None,
    }
    .scan(&["top".to_owned()], &omit, &follow, PinType::Fetch);

    assert_eq!(result.transitive.len(), 1);
    let child = &result.transitive[0];
    assert!(matches!(
        decide_input(&child.omit, &child.follow, Side::Tack, "doc-drop", true),
        InputDecision::Omit
    ));
    assert!(matches!(
        decide_input(&child.omit, &child.follow, Side::Tack, "local-drop", true),
        InputDecision::Omit
    ));
    assert!(matches!(
        decide_input(&child.omit, &child.follow, Side::Tack, "drop", true),
        InputDecision::Traverse
    ));
}

#[test]
fn nested_doc_all_follow_is_namespaced_to_its_pins() {
    let (omit, follow) = policies("[inputs.top]\nurl = \"github:o/top\"\n", &[], &[]);
    let result = ScanDocuments {
        flake_lock: None,
        tack_pins:  Some(
            "[all_follow]\ndep = \"repl\"\n[inputs.child]\nurl = \
             \"github:o/child\"\n[inputs.repl]\nurl = \"github:o/repl\"\n"
                .to_owned(),
        ),
        tack_lock:  None,
    }
    .scan(&["top".to_owned()], &omit, &follow, PinType::Fetch);

    let child = result
        .transitive
        .iter()
        .find(|target| target.path == ["top", "child"])
        .unwrap();
    assert!(matches!(
        decide_input(&child.omit, &child.follow, Side::Flake, "dep", false),
        InputDecision::Follow("top::repl")
    ));
    assert!(result.registry.iter().any(|entry| entry.0 == "top::repl"));
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
            "[inputs.child]\nurl = \"github:o/child\"\nexclude_follow = [\"foo\"]\n".to_owned(),
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
fn tack_follow_beats_omit_and_does_not_queue_original() {
    let (omit, follow) = policies(
        "[inputs.top]\nurl = \"github:o/top\"\nomit_inputs = \
         [\"child\"]\n[inputs.top.follows]\nchild = \"replacement\"\n",
        &[],
        &[],
    );
    let result = ScanDocuments {
        flake_lock: None,
        tack_pins:  Some("[inputs.child]\nurl = \"github:o/child\"\n".to_owned()),
        tack_lock:  None,
    }
    .scan(&["top".to_owned()], &omit, &follow, PinType::Fetch);

    assert!(result.findings.is_empty());
    assert!(result.transitive.is_empty());
    assert_eq!(result.followed.len(), 1);
    assert_eq!(result.followed[0].target, "replacement");
    assert!(result.followed[0].side == Side::Tack);
}

#[test]
fn scoped_exclusion_only_disables_matching_side() {
    let (omit, follow) = policies(
        "[inputs.top]\nurl = \"github:o/top\"\nexclude_follow = [\"flake:foo\"]\n",
        &[],
        &[("foo", "replacement")],
    );
    assert!(matches!(
        decide_input(&omit, &follow, Side::Flake, "foo", true),
        InputDecision::Traverse
    ));
    assert!(matches!(
        decide_input(&omit, &follow, Side::Tack, "foo", true),
        InputDecision::Follow("replacement")
    ));
    assert_eq!(
        global_follow_target(
            &BTreeMap::from([("foo".to_owned(), "replacement".to_owned())]),
            &BTreeSet::from(["flake:foo".to_owned()]),
            FollowSide::Tack,
            "foo",
        ),
        Some("replacement")
    );
}

#[test]
fn graph_resolution_failures_are_diagnostics() {
    let result = scan_flake(
        r#"{"root":"root","nodes":{"root":{"inputs":{"missing":"missing"}}}}"#,
        &OmitPolicy::default(),
        &FollowPolicy::default(),
    );

    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].file(), ScanFile::FlakeLock);
}

#[test]
fn scan_reports_tack_lock_parse_failure_and_continues() {
    let result = ScanDocuments {
        flake_lock: None,
        tack_pins:  Some(
            r#"
            [inputs.dep]
            url = "github:Owner/Repo"
            "#
            .to_owned(),
        ),
        tack_lock:  Some("{".to_owned()),
    }
    .scan(
        &["root".to_owned()],
        &OmitPolicy::default(),
        &FollowPolicy::default(),
        PinType::Fetch,
    );

    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].file(), ScanFile::TackLock);
}

#[test]
fn unwired_flake_pin_blocks_inherited_policy_with_diagnostic() {
    let (omit, follow) = policies("[inputs.top]\nurl = \"github:o/top\"\n", &["drop"], &[]);
    let docs = ScanDocuments {
        flake_lock: None,
        tack_pins:  Some("[inputs.drop]\nurl = \"github:o/drop\"\n".to_owned()),
        tack_lock:  None,
    };

    let unwired = docs.scan(&["top".to_owned()], &omit, &follow, PinType::Flake);
    assert_eq!(unwired.findings.len(), 1);
    assert!(
        unwired
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.to_string().contains("recomposable"))
    );

    let drilled = docs.scan(&["top".to_owned()], &omit, &follow, PinType::Fetch);
    assert!(drilled.findings.is_empty());
    assert!(drilled.diagnostics.is_empty());
}

#[test]
fn recomposable_flake_pin_receives_inherited_policy() {
    let (omit, follow) = policies("[inputs.top]\nurl = \"github:o/top\"\n", &["drop"], &[]);
    let result = ScanDocuments {
        flake_lock: None,
        tack_pins:  Some(
            "[tack]\nrecomposable = true\n[inputs.drop]\nurl = \"github:o/drop\"\n".to_owned(),
        ),
        tack_lock:  None,
    }
    .scan(&["top".to_owned()], &omit, &follow, PinType::Flake);

    assert!(result.findings.is_empty());
    assert!(result.diagnostics.is_empty());
}

#[test]
fn doc_tables_do_not_reach_the_docs_own_flake_side() {
    let result = ScanDocuments {
        flake_lock: Some(
            r#"{
                "root":"root",
                "nodes":{
                    "root":{"inputs":{"fdep":"fdep"}},
                    "fdep":{"locked":{"type":"github","owner":"o","repo":"fdep","rev":"1"}}
                }
            }"#
            .to_owned(),
        ),
        tack_pins:  Some(
            "[omit_inputs]\nnames = [\"fdep\"]\n[inputs.child]\nurl = \"github:o/child\"\n"
                .to_owned(),
        ),
        tack_lock:  None,
    }
    .scan(
        &["top".to_owned()],
        &OmitPolicy::default(),
        &FollowPolicy::default(),
        PinType::Flake,
    );

    // the doc's own tables govern its pins' subtrees, not its own flake.lock
    assert!(
        result
            .findings
            .iter()
            .any(|finding| finding.entry.name == "fdep")
    );
    let child = &result.transitive[0];
    assert!(matches!(
        decide_input(&child.omit, &child.follow, Side::Flake, "fdep", true),
        InputDecision::Omit
    ));
}

#[test]
fn fetch_pins_do_not_contribute_their_flake_lock() {
    let result = ScanDocuments {
        flake_lock: Some(
            r#"{
                "root":"root",
                "nodes":{
                    "root":{"inputs":{"fdep":"fdep"}},
                    "fdep":{"locked":{"type":"github","owner":"o","repo":"fdep","rev":"1"}}
                }
            }"#
            .to_owned(),
        ),
        tack_pins:  None,
        tack_lock:  None,
    }
    .scan(
        &["top".to_owned()],
        &OmitPolicy::default(),
        &FollowPolicy::default(),
        PinType::Fetch,
    );

    assert!(result.findings.is_empty());
}

#[test]
fn wildcard_exclusion_disables_global_follows() {
    for (rule, flake_followed, tack_followed) in [
        ("*", false, false),
        ("flake:*", false, true),
        ("tack:*", true, false),
    ] {
        let (omit, follow) = policies(
            &format!("[inputs.top]\nurl = \"github:o/top\"\nexclude_follow = [\"{rule}\"]\n"),
            &[],
            &[("foo", "replacement")],
        );
        for (side, followed) in [(Side::Flake, flake_followed), (Side::Tack, tack_followed)] {
            assert_eq!(
                matches!(
                    decide_input(&omit, &follow, side, "foo", true),
                    InputDecision::Follow("replacement")
                ),
                followed,
                "rule {rule:?} {side}"
            );
        }
    }
}

#[test]
fn explicit_keep_overrides_global_omit() {
    let (omit, follow) = policies(
        "[inputs.top]\nurl = \"github:o/top\"\nkeep_inputs = [\"dep\"]\n",
        &["dep"],
        &[],
    );
    let result = scan_flake(
        r#"{
            "root":"root",
            "nodes":{
                "root":{"inputs":{"dep":"dep"}},
                "dep":{"locked":{"type":"github","owner":"o","repo":"dep","rev":"1"}}
            }
        }"#,
        &omit,
        &follow,
    );

    assert_eq!(result.findings.len(), 1);
}
