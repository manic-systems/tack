// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    sync::{
        Arc,
        Mutex,
    },
};

use super::{
    TargetPin,
    coalesce_groups,
    scan::{
        FollowPolicy,
        LoadedDocuments,
        OmitPolicy,
        ScanDocuments,
        ScanTarget,
        SourceRef,
    },
    scan_routes,
    target_registry,
};
use crate::{
    commands::dedup::model::Side,
    lock::{
        LockFile,
        LockedNode,
    },
    pins::{
        Input,
        PinType,
        PinsDoc,
    },
    source::id::SourceId,
};

fn input(raw: &str) -> Input {
    PinsDoc::parse(raw)
        .unwrap()
        .inputs()
        .unwrap()
        .pop()
        .unwrap()
}

fn source(name: &str) -> SourceRef {
    SourceRef::Url(format!("github:test/{name}"))
}

fn target(path: &[&str], source_name: &str, omit: OmitPolicy, follow: FollowPolicy) -> ScanTarget {
    ScanTarget {
        path: path.iter().map(|part| (*part).to_owned()).collect(),
        source: source(source_name),
        submodules: false,
        pin_type: PinType::Flake,
        omit,
        follow,
        ancestors: BTreeSet::new(),
    }
}

fn target_key(source: SourceRef, submodules: bool) -> String {
    ScanTarget {
        path: Vec::new(),
        source,
        submodules,
        pin_type: PinType::Flake,
        omit: OmitPolicy::default(),
        follow: FollowPolicy::default(),
        ancestors: BTreeSet::new(),
    }
    .key()
}

fn pin(name: &str, rev: &str, omit: OmitPolicy, follow: FollowPolicy) -> TargetPin {
    TargetPin {
        identity: Some(SourceId::from_url(&format!("github:test/{name}")).unwrap()),
        rev: rev.to_owned(),
        lm: Some(10),
        source: source(name),
        submodules: false,
        pin_type: PinType::Flake,
        omit,
        follow,
    }
}

fn flake_dep(owner: &str, repo: &str, rev: &str) -> String {
    format!(
        r#"{{
            "root":"root",
            "nodes":{{
                "root":{{"inputs":{{"dep":"dep"}}}},
                "dep":{{"locked":{{"type":"github","owner":"{owner}","repo":"{repo}","rev":"{rev}"}}}}
            }}
        }}"#
    )
}

fn run_with_documents(
    frontier: Vec<ScanTarget>,
    targets: &BTreeMap<String, TargetPin>,
    documents: &BTreeMap<String, ScanDocuments>,
) -> (super::ScanOutcome, BTreeMap<String, usize>) {
    let calls = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
    let observed = Arc::clone(&calls);
    let outcome = scan_routes(frontier, targets, &|target| {
        let key = target.key();
        *observed.lock().unwrap().entry(key.clone()).or_default() += 1;
        Ok(LoadedDocuments {
            documents:   documents[&key].clone(),
            diagnostics: Vec::new(),
        })
    });
    let counts = calls.lock().unwrap().clone();
    (outcome, counts)
}

fn occurrence_signature(outcome: &super::ScanOutcome) -> Vec<(String, Vec<String>, String)> {
    outcome
        .groups
        .iter()
        .flat_map(|(id, entries)| {
            entries
                .iter()
                .map(|entry| (id.to_string(), entry.path.clone(), entry.rev.clone()))
        })
        .collect()
}

#[test]
fn route_order_does_not_change_policy_results_and_loads_once() {
    let shared = source("shared");
    let docs = BTreeMap::from([(
        target_key(shared, false),
        ScanDocuments::from_raw(Some(&flake_dep("test", "dep", "one")), None, None),
    )]);
    let omit_input =
        input("[inputs.omit]\nurl = \"github:test/shared\"\nomit_inputs = [\"dep\"]\n");
    let keep_input = input("[inputs.keep]\nurl = \"github:test/shared\"\n");
    let omitted = target(
        &["omit"],
        "shared",
        OmitPolicy::for_input(&BTreeSet::new(), &omit_input),
        FollowPolicy::default(),
    );
    let kept = target(
        &["keep"],
        "shared",
        OmitPolicy::for_input(&BTreeSet::new(), &keep_input),
        FollowPolicy::default(),
    );

    let (first, first_calls) =
        run_with_documents(vec![omitted.clone(), kept.clone()], &BTreeMap::new(), &docs);
    let (second, second_calls) = run_with_documents(vec![kept, omitted], &BTreeMap::new(), &docs);

    assert_eq!(occurrence_signature(&first), occurrence_signature(&second));
    assert_eq!(occurrence_signature(&first)[0].1, ["keep"]);
    assert_eq!(first_calls.values().copied().sum::<usize>(), 1);
    assert_eq!(second_calls.values().copied().sum::<usize>(), 1);
}

#[test]
fn submodule_setting_is_part_of_the_document_cache_key() {
    let plain = target(
        &["top"],
        "shared",
        OmitPolicy::default(),
        FollowPolicy::default(),
    );
    let mut with_submodules = plain.clone();
    with_submodules.submodules = true;
    let docs = BTreeMap::from([
        (plain.key(), ScanDocuments::from_raw(None, None, None)),
        (
            with_submodules.key(),
            ScanDocuments::from_raw(None, None, None),
        ),
    ]);

    let (_, calls) = run_with_documents(
        vec![plain.clone(), plain, with_submodules],
        &BTreeMap::new(),
        &docs,
    );

    assert_eq!(calls.len(), 2);
    assert!(calls.values().all(|count| *count == 1));
}

#[test]
fn same_source_at_two_paths_remains_two_occurrences() {
    let shared = source("shared");
    let docs = BTreeMap::from([(
        target_key(shared, false),
        ScanDocuments::from_raw(Some(&flake_dep("test", "dep", "one")), None, None),
    )]);
    let (outcome, _) = run_with_documents(
        vec![
            target(
                &["left"],
                "shared",
                OmitPolicy::default(),
                FollowPolicy::default(),
            ),
            target(
                &["right"],
                "shared",
                OmitPolicy::default(),
                FollowPolicy::default(),
            ),
        ],
        &BTreeMap::new(),
        &docs,
    );

    let paths = outcome
        .groups
        .values()
        .flatten()
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    assert_eq!(paths, [vec!["left".to_owned()], vec!["right".to_owned()]]);
}

#[test]
fn exact_occurrence_is_coalesced_with_maximum_timestamp() {
    let id = SourceId::from_url("github:test/dep").unwrap();
    let mut groups = BTreeMap::from([(id, vec![
        super::Entry {
            path: vec!["top".to_owned()],
            name: "dep".to_owned(),
            side: Side::Flake,
            rev:  "one".to_owned(),
            lm:   Some(1),
        },
        super::Entry {
            path: vec!["top".to_owned()],
            name: "dep".to_owned(),
            side: Side::Flake,
            rev:  "one".to_owned(),
            lm:   Some(2),
        },
    ])]);

    coalesce_groups(&mut groups);

    let entries = groups.values().next().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].lm, Some(2));
}

#[test]
fn follow_uses_target_identity_revision_and_policy() {
    let top_input = input(
        "[inputs.top]\nurl = \"github:test/top\"\n[inputs.top.follows]\nold = \"replacement\"\n",
    );
    let replacement_input = input(
        "[inputs.replacement]\nurl = \"github:test/replacement\"\nomit_inputs = \
         [\"drop\"]\n[inputs.replacement.follows]\nnested = \"leaf-target\"\n",
    );
    let top_follow = FollowPolicy::for_input(&BTreeMap::new(), &top_input);
    let replacement_omit = OmitPolicy::for_input(&BTreeSet::new(), &replacement_input);
    let replacement_follow = FollowPolicy::for_input(&BTreeMap::new(), &replacement_input);
    let mut leaf_target = pin(
        "leaf-target",
        "leaf-target-rev",
        OmitPolicy::default(),
        FollowPolicy::default(),
    );
    leaf_target.pin_type = PinType::Fixed;
    let registry = BTreeMap::from([
        (
            "replacement".to_owned(),
            pin(
                "replacement",
                "target-rev",
                replacement_omit,
                replacement_follow,
            ),
        ),
        ("leaf-target".to_owned(), leaf_target),
    ]);
    let top_lock = r#"{
        "root":"root",
        "nodes":{
            "root":{"inputs":{"old":"old"}},
            "old":{"locked":{"type":"github","owner":"test","repo":"original","rev":"original-rev"}}
        }
    }"#;
    let replacement_lock = r#"{
        "root":"root",
        "nodes":{
            "root":{"inputs":{"drop":"drop","keep":"keep","nested":"nested"}},
            "drop":{"locked":{"type":"github","owner":"test","repo":"drop","rev":"drop-rev"}},
            "keep":{"locked":{"type":"github","owner":"test","repo":"keep","rev":"keep-rev"}},
            "nested":{"locked":{"type":"github","owner":"test","repo":"nested-original","rev":"nested-original-rev"}}
        }
    }"#;
    let docs = BTreeMap::from([
        (
            target_key(source("top"), false),
            ScanDocuments::from_raw(Some(top_lock), None, None),
        ),
        (
            target_key(source("replacement"), false),
            ScanDocuments::from_raw(Some(replacement_lock), None, None),
        ),
    ]);

    let (outcome, _) = run_with_documents(
        vec![target(&["top"], "top", OmitPolicy::default(), top_follow)],
        &registry,
        &docs,
    );

    assert!(
        !outcome
            .groups
            .contains_key(&SourceId::from_url("github:test/original").unwrap())
    );
    let replacement = &outcome.groups[&SourceId::from_url("github:test/replacement").unwrap()][0];
    assert_eq!(replacement.rev, "target-rev");
    assert_eq!(replacement.path, ["top"]);
    assert_eq!(replacement.name, "old");
    assert!(
        !outcome
            .groups
            .contains_key(&SourceId::from_url("github:test/drop").unwrap())
    );
    assert!(
        outcome
            .groups
            .contains_key(&SourceId::from_url("github:test/keep").unwrap())
    );
    assert!(
        !outcome
            .groups
            .contains_key(&SourceId::from_url("github:test/nested-original").unwrap())
    );
    let nested = &outcome.groups[&SourceId::from_url("github:test/leaf-target").unwrap()][0];
    assert_eq!(nested.rev, "leaf-target-rev");
    assert_eq!(nested.path, ["top", "old"]);
    assert_eq!(nested.name, "nested");
}

#[test]
fn synthetic_target_and_missing_target_are_distinguished() {
    let raw_pins = "[inputs.top]\nurl = \"github:test/top\"\n";
    let follower = input(raw_pins);
    let all_follow = BTreeMap::from([("dep".to_owned(), "synthetic".to_owned())]);
    let follow = FollowPolicy::for_input(&all_follow, &follower);
    let pins = PinsDoc::parse(raw_pins).unwrap();
    let inputs = pins.inputs().unwrap();
    let mut lock = LockFile::new();
    lock.insert(
        "synthetic".to_owned(),
        LockedNode::new_github("test", "synthetic", "locked-rev", "hash", 10),
    );
    let registry = target_registry(
        &inputs,
        &lock,
        &pins.shorturls(),
        &BTreeSet::new(),
        &all_follow,
    );
    let top_lock = r#"{
        "root":"root",
        "nodes":{"root":{"inputs":{"dep":"dep"}},"dep":{"locked":{"type":"github","owner":"test","repo":"old","rev":"old"}}}
    }"#;
    let docs = BTreeMap::from([
        (
            target_key(source("top"), false),
            ScanDocuments::from_raw(Some(top_lock), None, None),
        ),
        (
            target_key(registry["synthetic"].source.clone(), false),
            ScanDocuments::from_raw(None, None, None),
        ),
    ]);
    let (resolved, _) = run_with_documents(
        vec![target(
            &["top"],
            "top",
            OmitPolicy::default(),
            follow.clone(),
        )],
        &registry,
        &docs,
    );
    assert!(
        resolved
            .groups
            .contains_key(&SourceId::from_url("github:test/synthetic").unwrap())
    );

    let (missing, _) = run_with_documents(
        vec![target(&["top"], "top", OmitPolicy::default(), follow)],
        &BTreeMap::new(),
        &docs,
    );
    assert!(missing.diagnostics.iter().any(|diagnostic| {
        diagnostic.to_string().contains("follow target 'synthetic'")
            && diagnostic.file().to_string() == ".tack/pins.toml"
    }));
}

#[test]
fn nested_all_follow_resolves_via_registered_targets() {
    let nested_pins = "[all_follow]\ndep = \"repl\"\n\n[inputs.mid]\nurl = \
                       \"github:test/mid\"\n\n[inputs.repl]\nurl = \"github:test/repl\"\n";
    let mid_lock = r#"{
        "root":"root",
        "nodes":{
            "root":{"inputs":{"dep":"dep"}},
            "dep":{"locked":{"type":"github","owner":"test","repo":"dep-orig","rev":"dep-rev"}}
        }
    }"#;
    let docs = BTreeMap::from([
        (
            target_key(source("top"), false),
            ScanDocuments::from_raw(None, Some(nested_pins), None),
        ),
        (
            target_key(source("mid"), false),
            ScanDocuments::from_raw(Some(mid_lock), None, None),
        ),
        (
            target_key(source("repl"), false),
            ScanDocuments::from_raw(None, None, None),
        ),
    ]);

    let (outcome, _) = run_with_documents(
        vec![target(
            &["top"],
            "top",
            OmitPolicy::default(),
            FollowPolicy::default(),
        )],
        &BTreeMap::new(),
        &docs,
    );

    assert!(outcome.diagnostics.is_empty());
    assert!(
        !outcome
            .groups
            .contains_key(&SourceId::from_url("github:test/dep-orig").unwrap())
    );
    let followed = &outcome.groups[&SourceId::from_url("github:test/repl").unwrap()];
    assert!(
        followed
            .iter()
            .any(|entry| entry.path == ["top", "mid"] && entry.name == "dep")
    );
}

#[test]
fn nested_omit_inputs_prune_grandchild_scans() {
    let nested_pins =
        "[omit_inputs]\nnames = [\"drop\"]\n\n[inputs.mid]\nurl = \"github:test/mid\"\n";
    let mid_lock = r#"{
        "root":"root",
        "nodes":{
            "root":{"inputs":{"drop":"drop","keep":"keep"}},
            "drop":{"locked":{"type":"github","owner":"test","repo":"drop","rev":"drop-rev"}},
            "keep":{"locked":{"type":"github","owner":"test","repo":"keep","rev":"keep-rev"}}
        }
    }"#;
    let docs = BTreeMap::from([
        (
            target_key(source("top"), false),
            ScanDocuments::from_raw(None, Some(nested_pins), None),
        ),
        (
            target_key(source("mid"), false),
            ScanDocuments::from_raw(Some(mid_lock), None, None),
        ),
    ]);

    let (outcome, _) = run_with_documents(
        vec![target(
            &["top"],
            "top",
            OmitPolicy::default(),
            FollowPolicy::default(),
        )],
        &BTreeMap::new(),
        &docs,
    );

    assert!(
        !outcome
            .groups
            .contains_key(&SourceId::from_url("github:test/drop").unwrap())
    );
    assert!(
        outcome
            .groups
            .contains_key(&SourceId::from_url("github:test/keep").unwrap())
    );
}

#[test]
fn cross_project_follow_cycle_terminates() {
    let a_input = input("[inputs.a]\nurl = \"github:test/a\"\n[inputs.a.follows]\nx = \"b\"\n");
    let b_input = input("[inputs.b]\nurl = \"github:test/b\"\n[inputs.b.follows]\ny = \"a\"\n");
    let a_follow = FollowPolicy::for_input(&BTreeMap::new(), &a_input);
    let b_follow = FollowPolicy::for_input(&BTreeMap::new(), &b_input);
    let registry = BTreeMap::from([
        (
            "a".to_owned(),
            pin("a", "a-rev", OmitPolicy::default(), a_follow.clone()),
        ),
        (
            "b".to_owned(),
            pin("b", "b-rev", OmitPolicy::default(), b_follow),
        ),
    ]);
    let a_lock = r#"{"root":"root","nodes":{"root":{"inputs":{"x":"x"}},"x":{"locked":{"type":"github","owner":"test","repo":"x","rev":"x"}}}}"#;
    let b_lock = r#"{"root":"root","nodes":{"root":{"inputs":{"y":"y"}},"y":{"locked":{"type":"github","owner":"test","repo":"y","rev":"y"}}}}"#;
    let docs = BTreeMap::from([
        (
            target_key(source("a"), false),
            ScanDocuments::from_raw(Some(a_lock), None, None),
        ),
        (
            target_key(source("b"), false),
            ScanDocuments::from_raw(Some(b_lock), None, None),
        ),
    ]);

    let (outcome, calls) = run_with_documents(
        vec![target(&["a"], "a", OmitPolicy::default(), a_follow)],
        &registry,
        &docs,
    );

    assert!(outcome.errors.is_empty());
    assert_eq!(calls.values().copied().sum::<usize>(), 2);
    assert_eq!(outcome.groups.values().flatten().count(), 2);
}
