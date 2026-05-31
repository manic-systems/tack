// SPDX-License-Identifier: EUPL-1.2

use std::{
    cmp,
    collections::{
        BTreeMap,
        BTreeSet,
        HashMap,
        HashSet,
    },
    env,
    fs,
    iter,
    mem,
    path::Path,
    result::Result as StdResult,
    sync::{
        Mutex,
        atomic::{
            AtomicUsize,
            Ordering,
        },
    },
};

use eyre::{
    Result,
    bail,
};

use crate::{
    fetch,
    fetch::{
        BranchComparison,
        CompareStatus,
    },
    history,
    lock,
    pins,
    pins::{
        PinType,
        Unpack,
    },
    project::{
        self,
        Project,
    },
    render,
    source::{
        Source,
        forge::Forge,
        id::SourceId,
    },
    ui::{
        Display,
        PinStatus,
    },
};

const STARTER_TOML: &str = include_str!("../../assets/pins.toml");
const RESOLVER_NIX: &str = include_str!("../../.tack/default.nix");
const SCAFFOLD_FLAKE: &str = include_str!("../../templates/default/flake.nix");
const MARKER: &str = "# tack-managed resolver.";

/// warn when the resolver still carries tack's marker but has drifted from the
/// bundled template. this is silent for forked resolvers who've stripped the
/// marker and when uninitialised, so it never nags people who own their copy.
pub fn warn_stale_resolver() {
    let path = Project::discover().resolver_path();
    if let Ok(current) = fs::read_to_string(&path)
        && current.contains(MARKER)
        && current != RESOLVER_NIX
    {
        eprintln!(
            "tack: resolver at {} is out of date. run `tack init --resolver` to update",
            path.display()
        );
    }
}

pub mod dedup;
mod edit;
mod init;
mod undo;
mod update;

pub fn init(force: bool, resolver_only: bool, flake: bool) -> Result<()> {
    init::init(force, resolver_only, flake)
}

pub fn add(
    name: &str,
    url: &str,
    pin_type: PinType,
    unpack: Option<Unpack>,
    dir_field: Option<&str>,
    submodules: bool,
    follows: &[(String, String)],
) -> Result<()> {
    edit::add(name, url, pin_type, unpack, dir_field, submodules, follows)
}

pub fn rm(name: &str) -> Result<()> {
    edit::rm(name)
}

pub fn alias(name: &str, template: Option<&str>, remove: bool) -> Result<()> {
    edit::alias(name, template, remove)
}

pub fn update(names: &[String], accept: bool) -> Result<()> {
    update::update(names, accept)
}

pub fn look(names: &[String], verbose: bool) -> Result<()> {
    update::look(names, verbose)
}

pub fn dedup() -> Result<()> {
    dedup::dedup()
}

pub fn undo(list: bool) -> Result<()> {
    undo::undo(list)
}

pub fn redo() -> Result<()> {
    undo::redo()
}

pub fn help() {
    init::help();
}

/// Disposition of a swallowed fetch result. Expected degraded-operation misses
/// vanish silently; fixable or suspicious failures return a cause string for
/// the caller to aggregate after any parallel batch or live spinner.
pub(in crate::commands) fn tolerate<T>(
    result: StdResult<T, fetch::FetchError>,
) -> (Option<T>, Option<String>) {
    match result {
        Ok(value) => (Some(value), None),
        Err(fetch::FetchError::NotFound { .. } | fetch::FetchError::Transport(_)) => (None, None),
        Err(err) => (None, Some(err.to_string())),
    }
}

fn select<'a>(inputs: &'a [pins::Input], names: &[String]) -> Vec<&'a pins::Input> {
    if names.is_empty() {
        return inputs.iter().collect();
    }
    let mut out = Vec::new();
    for n in names {
        match inputs.iter().find(|i| &i.name == n) {
            Some(i) => out.push(i),
            None => eprintln!("no input '{n}'"),
        }
    }
    out
}

fn top_map<T>(
    inputs: &[pins::Input],
    lock: &lock::LockFile,
    project: impl Fn(&lock::LockedNode) -> Option<T>,
) -> BTreeMap<String, T> {
    let declared = inputs
        .iter()
        .map(|inp| inp.name.as_str())
        .collect::<HashSet<&str>>();
    inputs
        .iter()
        .filter_map(|inp| {
            lock.get(&inp.name)
                .and_then(&project)
                .map(|val| (inp.name.clone(), val))
        })
        .chain(lock.iter().filter_map(|(key, node)| {
            (!declared.contains(key.as_str()))
                .then(|| project(node).map(|val| (key.clone(), val)))
                .flatten()
        }))
        .collect()
}

#[cfg(test)]
use self::{
    dedup::{
        Entry,
        MAX_COMPARE_JOBS,
        Mark,
        Side,
        apply_follows,
        classify,
        comparator,
        compare_jobs,
        group_diverges,
        pick_name,
        rev_last_modified,
    },
    edit::rm_in_dir,
    init::wires_overrides,
    update::LockObservation,
};

#[cfg(test)]
mod tests {
    use std::{
        collections::{
            BTreeMap,
            BTreeSet,
            HashMap,
        },
        fs,
        iter,
    };

    use super::{
        Entry,
        LockObservation,
        MAX_COMPARE_JOBS,
        Mark,
        Side,
        apply_follows,
        classify,
        comparator,
        compare_jobs,
        group_diverges,
        pick_name,
        rev_last_modified,
        rm_in_dir,
        tolerate,
        wires_overrides,
    };
    use crate::{
        fetch,
        lock,
        source::id::SourceId,
    };

    #[test]
    fn wires_overrides_ignores_comments() {
        assert!(wires_overrides(
            "outputs = { self, ... }@args: (import ./.tack) { overrides = args.tackOverrides or \
             {}; };"
        ));
        // a commented-out mention must not trip the recomposable flag
        assert!(!wires_overrides(
            "# threads args.tackOverrides through outputs\n{ }"
        ));
        assert!(!wires_overrides(
            "outputs = { self }: { }; # no tackOverrides here"
        ));
    }

    #[test]
    fn tolerate_swallows_absent_and_transport_silently() {
        assert_eq!(
            tolerate::<()>(Err(fetch::FetchError::NotFound { what: "x".into() })).1,
            None
        );
        assert_eq!(
            tolerate::<()>(Err(fetch::FetchError::Transport("down".into()))).1,
            None
        );
    }

    #[test]
    fn tolerate_surfaces_auth_and_upstream() {
        assert!(
            tolerate::<()>(Err(fetch::FetchError::Auth {
                what: "no token".into(),
            }))
            .1
            .is_some()
        );
        assert!(
            tolerate::<()>(Err(fetch::FetchError::Upstream("weird".into())))
                .1
                .is_some()
        );
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|&(alias, target)| (alias.to_owned(), target.to_owned()))
            .collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn tack_entry(path: &[&str], name: &str, rev: &str, lm: Option<u64>) -> Entry {
        Entry {
            side: Side::Tack,
            ..entry(path, name, rev, lm)
        }
    }

    fn entry(path: &[&str], name: &str, rev: &str, lm: Option<u64>) -> Entry {
        entry_full(path, name, rev, rev, lm)
    }

    fn entry_full(path: &[&str], name: &str, rev: &str, full_rev: &str, lm: Option<u64>) -> Entry {
        Entry {
            path: path.iter().map(|item| (*item).to_owned()).collect(),
            name: name.to_owned(),
            side: Side::Flake,
            rev: rev.to_owned(),
            full_rev: full_rev.to_owned(),
            lm,
        }
    }

    fn github_node(rev: &str) -> lock::LockedNode {
        lock::LockedNode::new_github("o", "r", rev, "sha256-n", 0)
    }

    fn node_rev(node: &lock::LockedNode) -> &str {
        node.rev().unwrap()
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
    fn pick_name_falls_back_to_shortest_alias_for_non_github() {
        let aliases = set(&["my-pin", "the-tarball"]);
        assert_eq!(pick_name(&source_id("https://x/y"), &aliases), "my-pin");
    }

    #[test]
    fn comparator_prefers_top_level_even_without_last_modified() {
        let entries = vec![
            entry(&["parent"], "aaa", "newer", Some(20)),
            entry(&[], "top", "top-rev", None),
        ];
        assert_eq!(
            comparator(&entries).map(|entry| (entry.rev.as_str(), entry.lm)),
            Some(("top-rev", None))
        );
    }

    #[test]
    fn comparator_uses_newest_known_transitive_then_deterministic_fallback() {
        let entries_with_known_time = vec![
            entry(&["parent"], "aaa", "unknown", None),
            entry(&["parent"], "bbb", "older", Some(10)),
            entry(&["parent"], "ccc", "newer", Some(20)),
        ];
        assert_eq!(
            comparator(&entries_with_known_time).map(|entry| (entry.rev.as_str(), entry.lm)),
            Some(("newer", Some(20)))
        );

        let entries_without_times = vec![
            entry(&["parent"], "bbb", "unknown-b", None),
            entry(&["parent"], "aaa", "unknown-a", None),
        ];
        assert_eq!(
            comparator(&entries_without_times).map(|entry| (entry.rev.as_str(), entry.lm)),
            Some(("unknown-a", None))
        );
    }

    #[test]
    fn auto_dedup_prefers_ahead_candidate_despite_older_timestamp() {
        let winner = LockObservation::choose(
            vec![
                LockObservation::new(300, github_node("base")),
                LockObservation::new(100, github_node("ahead")),
            ],
            |base, head| {
                match (node_rev(base), node_rev(head)) {
                    ("base", "ahead") => Some(super::CompareStatus::Ahead),
                    _ => None,
                }
            },
        )
        .unwrap();

        assert_eq!(node_rev(&winner), "ahead");
    }

    #[test]
    fn auto_dedup_keeps_base_when_candidate_is_behind_despite_newer_timestamp() {
        let winner = LockObservation::choose(
            vec![
                LockObservation::new(100, github_node("base")),
                LockObservation::new(500, github_node("behind")),
            ],
            |base, head| {
                match (node_rev(base), node_rev(head)) {
                    ("base", "behind") => Some(super::CompareStatus::Behind),
                    _ => None,
                }
            },
        )
        .unwrap();

        assert_eq!(node_rev(&winner), "base");
    }

    #[test]
    fn auto_dedup_falls_back_to_timestamp_for_diverged_histories() {
        let winner = LockObservation::choose(
            vec![
                LockObservation::new(100, github_node("base")),
                LockObservation::new(500, github_node("amended")),
            ],
            |base, head| {
                match (node_rev(base), node_rev(head)) {
                    ("base", "amended") => Some(super::CompareStatus::Diverged),
                    _ => None,
                }
            },
        )
        .unwrap();

        assert_eq!(node_rev(&winner), "amended");
    }

    #[test]
    fn group_divergence_uses_full_revs_not_display_revs() {
        let entries = vec![
            entry_full(
                &[],
                "base",
                "abcdef0",
                "abcdef0000000000000000000000000000000000",
                Some(10),
            ),
            entry_full(
                &["dep"],
                "head",
                "abcdef0",
                "abcdef0999999999999999999999999999999999",
                Some(20),
            ),
        ];

        assert!(group_diverges(&entries));
    }

    #[test]
    fn compare_jobs_use_full_revs_and_display_short_keys() {
        let mut groups = BTreeMap::new();
        groups.insert(source_id("github:o/r"), vec![
            entry_full(
                &[],
                "base",
                "1111111",
                "1111111111111111111111111111111111111111",
                Some(10),
            ),
            entry_full(
                &["dep"],
                "head",
                "2222222",
                "2222222222222222222222222222222222222222",
                Some(20),
            ),
        ]);

        let (jobs, capped) = compare_jobs(&groups);

        assert_eq!(capped, 0);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].base, "1111111111111111111111111111111111111111");
        assert_eq!(jobs[0].head, "2222222222222222222222222222222222222222");
    }

    #[test]
    fn compare_jobs_are_capped_before_network_work() {
        let mut entries = vec![entry_full(&[], "base", "base", "base-full", Some(0))];
        for i in 0..(MAX_COMPARE_JOBS + 5) {
            entries.push(entry_full(
                &["dep"],
                &format!("head-{i:03}"),
                &format!("h{i:06}"),
                &format!("head-full-{i:03}"),
                Some(u64::try_from(i).unwrap() + 1),
            ));
        }
        let mut groups = BTreeMap::new();
        groups.insert(source_id("github:o/r"), entries);

        let (jobs, capped) = compare_jobs(&groups);

        assert_eq!(jobs.len(), MAX_COMPARE_JOBS);
        assert_eq!(capped, 5);
    }

    #[test]
    fn classify_prefers_branch_status_over_misleading_timestamps() {
        let id = source_id("github:o/r");
        let entries = vec![
            entry(&[], "base", "base", Some(500)),
            entry(&["dep"], "head", "head", Some(100)),
        ];
        let compares =
            HashMap::from([((id.clone(), "head".to_owned()), super::CompareStatus::Ahead)]);

        let mark = classify(
            &id,
            "head",
            comparator(&entries),
            &rev_last_modified(&entries),
            &compares,
        );

        assert_eq!(mark, Mark::Ahead);
    }

    #[test]
    fn classify_reports_diverged_branch_status() {
        let id = source_id("github:o/r");
        let entries = vec![
            entry(&[], "base", "base", Some(100)),
            entry(&["dep"], "head", "head", Some(200)),
        ];
        let compares = HashMap::from([(
            (id.clone(), "head".to_owned()),
            super::CompareStatus::Diverged,
        )]);

        let mark = classify(
            &id,
            "head",
            comparator(&entries),
            &rev_last_modified(&entries),
            &compares,
        );

        assert_eq!(mark, Mark::Diverged);
    }

    #[test]
    fn classify_distinguishes_timestamp_fallback() {
        let id = source_id("github:o/r");
        let entries = vec![
            entry(&[], "base", "base", Some(100)),
            entry(&["dep"], "head", "head", Some(200)),
        ];
        let compares = HashMap::<(SourceId, String), super::CompareStatus>::new();

        let mark = classify(
            &id,
            "head",
            comparator(&entries),
            &rev_last_modified(&entries),
            &compares,
        );

        assert_eq!(mark, Mark::DatedNewer);
    }

    #[test]
    fn apply_follows_syncs_rev_full_rev_and_lm_to_target() {
        let id = source_id("github:o/r");
        let mut groups = BTreeMap::new();
        groups.insert(id.clone(), vec![
            entry(&[], "nixpkgs", "newrev", Some(100)),
            // a transitive input that follows nixpkgs, carrying its own stale rev
            // and timestamp from before the follow was applied
            entry(&["dep"], "nixpkgs-lib", "oldrev", Some(50)),
        ]);
        let by_name = BTreeMap::new(); // top resolves via [all_follow], not a parent's follows
        let all_follow = map(&[("nixpkgs-lib", "nixpkgs")]);
        let top_revs = map(&[("nixpkgs", "newrev")]);
        let top_full_revs = map(&[("nixpkgs", "newrev-full")]);
        let top_lms = iter::once(("nixpkgs".to_owned(), 100_u64)).collect();

        apply_follows(
            &mut groups,
            &by_name,
            &all_follow,
            &top_revs,
            &top_full_revs,
            &top_lms,
        );

        let followed = &groups[&id][1];
        assert_eq!(followed.rev, "newrev");
        assert_eq!(followed.full_rev, "newrev-full");
        // lm should track the target rather than keeping the stale 50
        assert_eq!(followed.lm, Some(100));
    }

    #[test]
    fn apply_follows_honors_scoped_all_follow_per_side() {
        // an upstream tack pin `dep`, recorded as a tack-side finding
        let id = source_id("github:o/r");
        let mut groups = BTreeMap::new();
        groups.insert(id.clone(), vec![tack_entry(
            &["parent"],
            "dep",
            "oldrev",
            Some(50),
        )]);
        let by_name = BTreeMap::new();
        let top_revs = map(&[("replacement", "newrev")]);
        let top_full_revs = map(&[("replacement", "newrev-full")]);
        let top_lms = iter::once(("replacement".to_owned(), 100_u64)).collect();

        // a `flake:`-scoped rule must not touch a tack-side entry
        let flake_rule = map(&[("flake:dep", "replacement")]);
        apply_follows(
            &mut groups,
            &by_name,
            &flake_rule,
            &top_revs,
            &top_full_revs,
            &top_lms,
        );
        assert_eq!(groups[&id][0].rev, "oldrev");

        // the matching `tack:`-scoped rule aligns it onto the target
        let tack_rule = map(&[("tack:dep", "replacement")]);
        apply_follows(
            &mut groups,
            &by_name,
            &tack_rule,
            &top_revs,
            &top_full_revs,
            &top_lms,
        );
        assert_eq!(groups[&id][0].rev, "newrev");
        assert_eq!(groups[&id][0].full_rev, "newrev-full");
        assert_eq!(groups[&id][0].lm, Some(100));
    }

    #[test]
    fn rm_removes_orphaned_lock_entry() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pins.toml"), "[inputs]\n").unwrap();
        fs::write(
            dir.path().join("pins.lock.json"),
            r#"{"gone":{"type":"github","owner":"o","repo":"r","rev":"bad","narHash":"sha256-x"}}"#,
        )
        .unwrap();

        assert_eq!(rm_in_dir(dir.path(), "gone").unwrap(), (false, true));
        assert_eq!(
            fs::read_to_string(dir.path().join("pins.toml")).unwrap(),
            "[inputs]\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("pins.lock.json")).unwrap(),
            "{}\n"
        );
    }

    #[test]
    fn rm_errors_when_pin_and_lock_are_missing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pins.toml"), "[inputs]\n").unwrap();
        fs::write(dir.path().join("pins.lock.json"), "{}\n").unwrap();

        let err = rm_in_dir(dir.path(), "missing").unwrap_err().to_string();
        assert_eq!(err, "no input 'missing'");
    }
}
