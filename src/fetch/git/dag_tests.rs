// SPDX-License-Identifier: EUPL-1.2

use super::{
    compare_status,
    fetch_scan_files,
};
use crate::fetch::{
    CompareStatus,
    git::test_remote::{
        LocalRemote,
        Node,
    },
};

#[test]
fn compares_file_remote_topology() {
    let mut linear = LocalRemote::new();
    let base = linear.commit("one\n", "one");
    let head = linear.commit("one\ntwo\n", "two");
    let linear_url = linear.url();

    let mut diverged = LocalRemote::new();
    let root = diverged.commit("root\n", "root");
    let old = diverged.commit("old\n", "old");
    diverged.reset_to(&root);
    let new = diverged.commit("new\n", "new");
    let diverged_url = diverged.url();

    assert_eq!(
        compare_status(&linear_url, &base, &head).unwrap(),
        Some(CompareStatus::Ahead)
    );
    assert_eq!(
        compare_status(&linear_url, &head, &base).unwrap(),
        Some(CompareStatus::Behind)
    );
    assert_eq!(
        compare_status(&diverged_url, &old, &new).unwrap(),
        Some(CompareStatus::Diverged)
    );
}

#[test]
fn sparse_scan_reads_regular_files() {
    let mut remote = LocalRemote::new();
    let rev = remote.commit_nodes(
        vec![
            ("flake.lock", Node::Blob("{}")),
            (
                ".tack",
                Node::Dir(vec![("pins.toml", Node::Blob("[inputs]\n"))]),
            ),
            ("README", Node::Blob("hi")),
        ],
        "scan",
    );

    let files = fetch_scan_files(&remote.url(), &rev, &[
        "flake.lock",
        ".tack/pins.toml",
        ".tack/pins.lock.json",
    ])
    .unwrap();

    assert_eq!(files, vec![
        Some("{}".to_owned()),
        Some("[inputs]\n".to_owned()),
        None
    ]);
}

#[test]
fn sparse_scan_ignores_hostile_entry_kinds() {
    let mut remote = LocalRemote::new();
    let rev = remote.commit_nodes(
        vec![
            ("flake.lock", Node::Link("/etc/passwd")),
            (".tack", Node::Blob("not a tree")),
        ],
        "hostile",
    );
    let mut nested = LocalRemote::new();
    let nested_rev = nested.commit_nodes(
        vec![
            ("flake.lock", Node::Dir(vec![("x", Node::Blob("{}"))])),
            (
                ".tack",
                Node::Dir(vec![("pins.toml", Node::Link("../flake.lock"))]),
            ),
        ],
        "nested",
    );
    let paths = ["flake.lock", ".tack/pins.toml"];

    assert_eq!(
        fetch_scan_files(&remote.url(), &rev, &paths).unwrap(),
        vec![None, None]
    );
    assert_eq!(
        fetch_scan_files(&nested.url(), &nested_rev, &paths).unwrap(),
        vec![None, None]
    );
}

#[test]
fn sparse_scan_rejects_malformed_rev() {
    let remote = LocalRemote::new();

    fetch_scan_files(&remote.url(), "main", &["flake.lock"]).unwrap_err();
}
