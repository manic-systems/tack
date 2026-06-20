// SPDX-License-Identifier: EUPL-1.2

use std::collections::{
    BTreeMap,
    HashMap,
};

use super::{
    MAX_COMPARE_JOBS,
    classify,
    comparator,
    compare_jobs,
    rev_last_modified,
};
use crate::{
    commands::dedup::model::{
        Entry,
        Side,
    },
    fetch::CompareStatus,
    report::Mark,
    source::id::SourceId,
};

fn entry(path: &[&str], name: &str, rev: &str, lm: Option<u64>) -> Entry {
    Entry {
        path: path.iter().map(|item| (*item).to_owned()).collect(),
        name: name.to_owned(),
        side: Side::Flake,
        rev: rev.to_owned(),
        lm,
    }
}

fn source_id(str: &str) -> SourceId {
    SourceId::from_url(str).unwrap()
}

#[test]
fn compare_jobs_are_capped_before_network_work() {
    let mut entries = vec![entry(&[], "base", "base-full", Some(0))];
    for i in 0..(MAX_COMPARE_JOBS + 5) {
        entries.push(entry(
            &["dep"],
            &format!("head-{i:03}"),
            &format!("head-full-{i:03}"),
            Some(u64::try_from(i).unwrap() + 1),
        ));
    }
    let mut groups = BTreeMap::new();
    groups.insert(source_id("github:o/r"), entries);

    let (jobs, capped) = compare_jobs(&groups);

    assert_eq!(jobs.len(), MAX_COMPARE_JOBS);
    assert_eq!(capped, 5);
}

#[test]
fn comparator_prefers_top_level_pin_over_newer_transitive() {
    // A declared pin (empty path) wins even when a transitive entry has a newer lm.
    let declared = entry(&[], "nixpkgs", "rev-declared", Some(100));
    let transitive = entry(&["dep"], "nixpkgs", "rev-transitive", Some(9999));
    let entries = vec![declared, transitive];

    let chosen = comparator(&entries).unwrap();
    assert_eq!(chosen.rev, "rev-declared");
}

#[test]
fn classify_prefers_branch_status_over_timestamps() {
    let id = source_id("github:o/r");
    let entries = vec![
        entry(&[], "base", "base", Some(500)),
        entry(&["dep"], "head", "head", Some(100)),
    ];
    let compares = HashMap::from([(
        id.clone(),
        HashMap::from([("head".to_owned(), CompareStatus::Ahead)]),
    )]);

    let mark = classify(
        &id,
        "head",
        comparator(&entries),
        &rev_last_modified(&entries),
        &compares,
    );

    assert_eq!(mark, Mark::Ahead);
}
