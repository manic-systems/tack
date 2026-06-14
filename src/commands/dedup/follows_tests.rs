// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::BTreeMap,
    iter,
};

use super::apply_follows;
use crate::{
    commands::dedup::model::{
        Entry,
        Side,
    },
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

#[test]
fn apply_follows_syncs_rev_and_lm_to_target() {
    let id = SourceId::from_url("github:o/r").unwrap();
    let mut groups = BTreeMap::from([(id.clone(), vec![
        entry(&[], "nixpkgs", "newrev-full", Some(100)),
        entry(&["dep"], "nixpkgs-lib", "oldrev", Some(50)),
    ])]);
    let all_follow = BTreeMap::from([("nixpkgs-lib".to_owned(), "nixpkgs".to_owned())]);
    let top_revs = BTreeMap::from([("nixpkgs".to_owned(), "newrev-full".to_owned())]);
    let top_lms = iter::once(("nixpkgs".to_owned(), 100_u64)).collect();

    apply_follows(
        &mut groups,
        &BTreeMap::new(),
        &all_follow,
        &top_revs,
        &top_lms,
    );

    let followed = &groups[&id][1];
    assert_eq!(followed.rev, "newrev-full");
    assert_eq!(followed.lm, Some(100));
}
