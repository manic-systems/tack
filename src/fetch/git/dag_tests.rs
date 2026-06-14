// SPDX-License-Identifier: EUPL-1.2

use super::compare_status;
use crate::fetch::{
    CompareStatus,
    git::test_remote::LocalRemote,
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
