// SPDX-License-Identifier: EUPL-1.2

use serde_json::json;

use super::{
    fetch_pin,
    git,
    git_pin_from_checkout,
    git_revision_of,
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

#[test]
fn tarball_with_rev_drops_last_modified() {
    let serialized = serde_json::to_value(LockedNode::new_tarball_with_rev(
        "https://host/x.tar.xz",
        "9ae611a455b90cf061d8f332b977e387bda8e1ca",
        "sha256-n",
    ))
    .unwrap();
    assert_eq!(
        serialized,
        json!({
            "type": "tarball",
            "url": "https://host/x.tar.xz",
            "rev": "9ae611a455b90cf061d8f332b977e387bda8e1ca",
            "narHash": "sha256-n"
        })
    );
}

#[test]
fn git_revision_read_trimmed_and_validated() {
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    assert_eq!(git_revision_of(dir.path()), None);

    let rev = "9ae611a455b90cf061d8f332b977e387bda8e1ca";
    fs::write(dir.path().join(".git-revision"), format!("{rev}\n")).unwrap();
    assert_eq!(git_revision_of(dir.path()).as_deref(), Some(rev));

    fs::write(dir.path().join(".git-revision"), "not a rev").unwrap();
    assert_eq!(git_revision_of(dir.path()), None);
}
