// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::BTreeMap,
    iter,
};

use super::apply_follows;
use crate::{
    commands::dedup::model::{
        Entry,
        Identity,
        IdentityKind,
        Side,
    },
    source::id::SourceId,
};

fn rev(value: &str) -> Identity {
    Identity {
        kind:  IdentityKind::Rev,
        value: value.to_owned(),
    }
}

fn entry(path: &[&str], name: &str, value: &str, lm: Option<u64>) -> Entry {
    Entry {
        path: path.iter().map(|item| (*item).to_owned()).collect(),
        name: name.to_owned(),
        side: Side::Flake,
        identity: Some(rev(value)),
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
    let top_revs = BTreeMap::from([("nixpkgs".to_owned(), rev("newrev-full"))]);
    let top_lms = iter::once(("nixpkgs".to_owned(), 100_u64)).collect();

    apply_follows(
        &mut groups,
        &BTreeMap::new(),
        &all_follow,
        &top_revs,
        &top_lms,
    );

    let followed = &groups[&id][1];
    assert_eq!(followed.identity, Some(rev("newrev-full")));
    assert_eq!(followed.lm, Some(100));
}
