// SPDX-License-Identifier: EUPL-1.2

use super::{
    LockObservation,
    restrict_to_seed_identity,
};
use crate::{
    fetch::CompareStatus,
    lock::{
        LockedNode,
        PathFingerprint,
    },
};

fn github_node_in(owner: &str, repo: &str, rev: &str) -> LockedNode {
    LockedNode::new_github(owner, repo, rev, "sha256-n", 0)
}

fn node_rev(node: &LockedNode) -> &str {
    node.forge_rev().unwrap()
}

#[test]
fn auto_dedup_prefers_branch_status_over_timestamp() {
    let winner = LockObservation::choose(
        vec![
            LockObservation::new(300, github_node_in("o", "r", "base")),
            LockObservation::new(100, github_node_in("o", "r", "ahead")),
        ],
        |base, head| {
            match (node_rev(base), node_rev(head)) {
                ("base", "ahead") => Some(CompareStatus::Ahead),
                _ => None,
            }
        },
    )
    .unwrap();

    assert_eq!(node_rev(&winner), "ahead");
}

#[test]
fn restrict_to_seed_identity_drops_foreign_repositories() {
    let mut obs = vec![
        LockObservation::new(100, github_node_in("o", "r", "current")),
        LockObservation::new(900, github_node_in("fork", "r", "foreign")),
        LockObservation::new(800, github_node_in("o", "r", "sibling")),
    ];
    restrict_to_seed_identity(&mut obs);

    let revs = obs
        .iter()
        .map(|entry| node_rev(&entry.node))
        .collect::<Vec<_>>();
    assert_eq!(revs, vec!["current", "sibling"]);
}

#[test]
fn restrict_to_seed_identity_keeps_path_purity() {
    let mut obs = vec![
        LockObservation::new(
            100,
            LockedNode::new_path("/tmp/dep", Some("sha256-h".to_owned())),
        ),
        LockObservation::new(
            900,
            LockedNode::new_path_with_fingerprint("/tmp/dep", PathFingerprint {
                last_modified: 1,
                mtime_nanos:   1,
                tree_size:     1,
                tree_entries:  1,
            }),
        ),
    ];
    restrict_to_seed_identity(&mut obs);

    assert_eq!(obs.len(), 1);
    assert!(obs[0].node.hash().is_some());
}
