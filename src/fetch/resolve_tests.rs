// SPDX-License-Identifier: EUPL-1.2

use serde_json::json;

use super::{
    fetch_pin,
    git,
    git_pin_from_checkout,
};
use crate::{
    lock::LockedNode,
    source::Source,
};

fn node(value: serde_json::Value) -> LockedNode {
    LockedNode::from_value(value).unwrap()
}

#[test]
fn path_pin_locks_absolute_targets_with_a_nar_hash() {
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("f"), "hello").unwrap();

    let absolute = Source::Path {
        path: tmp.path().to_string_lossy().into_owned(),
    };
    let (absolute_locked, _) = fetch_pin(&absolute, false).unwrap();
    assert!(
        absolute_locked
            .hash()
            .is_some_and(|hash| hash.starts_with("sha256-"))
    );

    let relative = Source::Path {
        path: "../vendor/dep".to_owned(),
    };
    let (relative_locked, _) = fetch_pin(&relative, false).unwrap();
    assert_eq!(relative_locked.hash(), None);
}

#[test]
fn gitlab_git_url_checkout_stays_generic_git_lock() {
    let source = "git+https://gitlab.com/Group/Repo.git?ref=main&rev=abc123"
        .parse::<Source>()
        .unwrap();
    let (fetched, _) = git_pin_from_checkout(
        &source,
        git::PinCheckout {
            rev:           "abc123".to_owned(),
            nar_hash:      "sha256-n".to_owned(),
            last_modified: 1_700,
            refname:       "refs/heads/main".to_owned(),
        },
        true,
    )
    .unwrap();

    assert_eq!(
        fetched,
        node(json!({
            "type": "git",
            "url": "https://gitlab.com/Group/Repo.git",
            "ref": "refs/heads/main",
            "rev": "abc123",
            "narHash": "sha256-n",
            "lastModified": 1_700_i64,
            "submodules": true
        }))
    );
}
