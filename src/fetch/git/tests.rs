// SPDX-License-Identifier: EUPL-1.2

use std::fs;

use super::{
    fetch_tree_into,
    test_remote::LocalRemote,
};

#[test]
fn pinned_rev_reachable_only_off_named_ref_is_found() {
    let tmp = tempfile::tempdir().unwrap();
    let mut remote = LocalRemote::new();
    remote.commit("main\n", "main");
    remote.branch_from_current("refs/heads/feature");
    let pinned = remote.commit("feature\n", "feature");
    let dest = tmp.path().join("out");
    fs::create_dir_all(&dest).unwrap();

    // ref names main, but the pinned rev only lives on feature, so the fetch must
    // widen past the named ref to find it
    fetch_tree_into(&remote.url(), Some("main"), Some(&pinned), false, &dest).unwrap();

    assert_eq!(
        fs::read_to_string(dest.join("file.txt")).unwrap(),
        "feature\n"
    );
}
