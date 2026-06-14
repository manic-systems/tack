// SPDX-License-Identifier: EUPL-1.2

use super::{
    LockObservation,
    restrict_to_seed_identity,
};
use crate::{
    fetch::CompareStatus,
    lock::LockedNode,
};

fn github_node_in(owner: &str, repo: &str, rev: &str) -> LockedNode {
    LockedNode::new_github(owner, repo, rev, "sha256-n", 0)
}

fn node_rev(node: &LockedNode) -> &str {
    node.rev().unwrap()
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
