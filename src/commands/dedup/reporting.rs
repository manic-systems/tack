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
    fetch::github::CompareStatus,
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
        // single source or already aligned: nothing to show
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

/// suggested top-level name for a transitive-only group: forge repo basename,
/// else shortest alias
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::pick_name;
    use crate::source::id::SourceId;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn source_id(str: &str) -> SourceId {
        SourceId::from_url(str).unwrap()
    }

    #[test]
    fn pick_name_strips_dot_nix_and_flattens_dots() {
        assert_eq!(
            pick_name(
                &source_id("github:cachix/git-hooks.nix"),
                &set(&["git-hooks"])
            ),
            "git-hooks"
        );
        assert_eq!(
            pick_name(
                &source_id("github:nix-community/nixpkgs.lib"),
                &set(&["nixpkgs-lib"])
            ),
            "nixpkgs-lib"
        );
    }

    #[test]
    fn pick_name_uses_gitlab_repo_basename() {
        assert_eq!(
            pick_name(
                &source_id("gitlab:Veloren%2Fdev/rfcs.nix"),
                &set(&["veloren-rfcs", "rfcs"])
            ),
            "rfcs"
        );
        assert_eq!(
            pick_name(
                &source_id("git+https://gitlab.com/NixOS/nixpkgs.lib.git"),
                &set(&["nixpkgs-lib"])
            ),
            "nixpkgs-lib"
        );
    }

    #[test]
    fn pick_name_falls_back_to_shortest_alias_without_repo_coordinates() {
        let aliases = set(&["my-pin", "the-tarball"]);
        assert_eq!(pick_name(&source_id("https://x/y"), &aliases), "my-pin");
    }
}
