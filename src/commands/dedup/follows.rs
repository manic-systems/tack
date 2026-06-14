// SPDX-License-Identifier: EUPL-1.2

use std::collections::BTreeMap;

use super::model::{
    Entry,
    Side,
};
use crate::{
    pins,
    source::id::SourceId,
};

fn follow_target(
    path: &[String],
    name: &str,
    side: Side,
    top_input: Option<&pins::Input>,
    all_follow: &BTreeMap<String, String>,
    top_revs: &BTreeMap<String, String>,
) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let scoped = format!("{}:{name}", side.as_str());
    let excluded = top_input.is_some_and(|inp| inp.excludes.contains(name));
    if !excluded
        && let Some(target) = all_follow.get(name).or_else(|| all_follow.get(&scoped))
        && top_revs.contains_key(target)
    {
        return Some(target.clone());
    }
    if path.len() == 1
        && let Some(inp) = top_input
        && let Some(target) = inp.follows.get(name).or_else(|| inp.follows.get(&scoped))
        && top_revs.contains_key(target)
    {
        return Some(target.clone());
    }
    None
}

pub(super) fn apply_follows(
    groups: &mut BTreeMap<SourceId, Vec<Entry>>,
    by_name: &BTreeMap<&str, &pins::Input>,
    all_follow: &BTreeMap<String, String>,
    top_revs: &BTreeMap<String, String>,
    top_lms: &BTreeMap<String, u64>,
) {
    for entry in groups.values_mut().flatten() {
        let top = entry
            .path
            .first()
            .and_then(|name| by_name.get(name.as_str()).copied());
        let Some(target) = follow_target(
            &entry.path,
            &entry.name,
            entry.side,
            top,
            all_follow,
            top_revs,
        ) else {
            continue;
        };
        if let Some(rev) = top_revs.get(&target) {
            entry.rev.clone_from(rev);
        }
        entry.lm = top_lms.get(&target).copied();
    }
}

#[cfg(test)]
mod tests {
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
}
