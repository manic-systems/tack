// SPDX-License-Identifier: EUPL-1.2

use std::collections::{
    BTreeMap,
    BTreeSet,
    HashMap,
};

use super::{
    compare::{
        classify,
        comparator,
        entry_compare_rev,
        group_diverges,
        rev_last_modified,
    },
    model::Entry,
};
use crate::{
    fetch::CompareStatus,
    report::{
        DedupGroup,
        DedupReport,
        FollowSuggestions,
        NameSources,
        RevGroup,
    },
    source::id::SourceId,
};

type SourcesByRev<'a> = BTreeMap<&'a str, BTreeMap<&'a str, Vec<Vec<String>>>>;

fn group_sources_by_rev(entries: &[Entry]) -> SourcesByRev<'_> {
    let mut by_rev = BTreeMap::<&str, BTreeMap<&str, Vec<Vec<String>>>>::new();
    for entry in entries {
        let names = by_rev.entry(entry_compare_rev(entry)).or_default();
        names
            .entry(entry.name.as_str())
            .or_default()
            .push(entry.path.clone());
    }
    by_rev
}

pub(super) fn build_report(
    groups: &BTreeMap<SourceId, Vec<Entry>>,
    all_follow: &BTreeMap<String, String>,
    compares: &HashMap<(SourceId, String), CompareStatus>,
) -> DedupReport {
    let mut follows = FollowSuggestions::default();
    let mut report_groups = Vec::<DedupGroup>::new();

    for (id, entries) in groups {
        if !group_diverges(entries) {
            continue;
        }

        let by_rev = group_sources_by_rev(entries);
        let comp = comparator(entries);
        let lm_of = rev_last_modified(entries);
        let revs = by_rev
            .into_iter()
            .map(|(rev, name_map)| {
                let name_sources = name_map
                    .into_iter()
                    .map(|(name, sources)| {
                        NameSources {
                            name: name.to_owned(),
                            sources,
                        }
                    })
                    .collect::<Vec<_>>();
                RevGroup {
                    rev:   rev.to_owned(),
                    mark:  classify(id, rev, comp, &lm_of, compares),
                    names: name_sources,
                }
            })
            .collect::<Vec<_>>();

        let top_name = entries
            .iter()
            .filter(|entry| entry.path.is_empty())
            .map(|entry| entry.name.as_str())
            .min();
        if let Some(top) = top_name {
            for entry in entries {
                if !entry.path.is_empty() && !all_follow.contains_key(&entry.name) {
                    follows.pin.insert(entry.name.clone(), top.to_owned());
                }
            }
        } else {
            let aliases = entries
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<BTreeSet<String>>();
            let canonical = pick_name(id, &aliases);
            for alias in &aliases {
                if !all_follow.contains_key(alias) {
                    follows.auto.insert(alias.clone(), canonical.clone());
                }
            }
        }

        report_groups.push(DedupGroup {
            id: id.to_string(),
            count: entries.len(),
            revs,
        });
    }

    DedupReport {
        groups: report_groups,
        follows,
    }
}

pub(super) fn pick_name(id: &SourceId, aliases: &BTreeSet<String>) -> String {
    if let Some(repo) = id.repo_name() {
        return repo.trim_end_matches(".nix").replace('.', "-");
    }
    aliases
        .iter()
        .min_by_key(|name| (name.len(), name.as_str()))
        .cloned()
        .unwrap_or_default()
}
