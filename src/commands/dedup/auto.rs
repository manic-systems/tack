// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
        HashSet,
    },
    mem::discriminant,
};

use super::scan::{
    strip_disambiguator,
    try_raw_file,
};
use crate::{
    commands::tolerate,
    dispatcher,
    fetch::{
        CompareStatus,
        compare_planner::CompareSession,
    },
    lock::{
        self,
        LockedNode,
    },
    pins::{
        self,
        PinType,
    },
    scan_diagnostic::{
        ScanDiagnostic,
        ScanFile,
    },
    source::id::SourceId,
};

const AUTO_DEDUP_SCAN_IN_FLIGHT: usize = 16;

#[derive(Default)]
pub struct AutoDedupReport {
    pub changed:               bool,
    pub scan_diagnostics:      BTreeSet<ScanDiagnostic>,
    pub surfaced_fetch_causes: BTreeSet<String>,
}

struct AutoFollowAliases {
    targets_by_alias: BTreeMap<String, String>,
}

impl AutoFollowAliases {
    fn from_inputs(inputs: &[pins::Input], all_follow: &BTreeMap<String, String>) -> Self {
        let input_names = inputs
            .iter()
            .map(|i| i.name.as_str())
            .collect::<HashSet<&str>>();

        Self {
            targets_by_alias: all_follow
                .iter()
                .filter(|&(_, target)| !input_names.contains(target.as_str()))
                .filter_map(|(alias, target)| {
                    Some((
                        pins::FollowAlias::new(alias).flake_side()?.to_owned(),
                        target.clone(),
                    ))
                })
                .collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.targets_by_alias.is_empty()
    }

    fn target_for(&self, alias: &str) -> Option<&String> {
        self.targets_by_alias.get(alias)
    }

    fn targets(&self) -> impl Iterator<Item = &String> {
        self.targets_by_alias.values()
    }
}

#[derive(Clone)]
struct LockObservation {
    last_modified: i64,
    node:          LockedNode,
}

impl LockObservation {
    #[cfg(test)]
    const fn new(last_modified: i64, node: LockedNode) -> Self {
        Self {
            last_modified,
            node,
        }
    }

    fn from_node(node: LockedNode) -> Self {
        let last_modified = node
            .last_modified()
            .and_then(|lm| i64::try_from(lm).ok())
            .unwrap_or(0);
        Self {
            last_modified,
            node,
        }
    }

    fn choose<C>(observations: Vec<Self>, mut compare: C) -> Option<LockedNode>
    where
        C: FnMut(&LockedNode, &LockedNode) -> Option<CompareStatus>,
    {
        let mut iter = observations.into_iter();
        let mut winner = iter.next()?;
        for candidate in iter {
            match compare(&winner.node, &candidate.node) {
                Some(CompareStatus::Ahead) => winner = candidate,
                Some(CompareStatus::Behind | CompareStatus::Identical) => {},
                Some(CompareStatus::Diverged) | None => {
                    if candidate.last_modified > winner.last_modified {
                        winner = candidate;
                    }
                },
            }
        }
        Some(winner.node)
    }
}

#[derive(Default)]
struct ScanBatch {
    observations: Vec<(String, LockObservation)>,
    diagnostics:  BTreeSet<ScanDiagnostic>,
}

/// for each `[all_follow]` target that isn't a declared input, write the
/// freshest transitive observation from top-level flake.locks, scanning only
/// the inputs in `only`
pub fn auto_dedup_scoped(
    inputs: &[pins::Input],
    all_follow: &BTreeMap<String, String>,
    lock: &mut lock::LockFile,
    only: &[String],
) -> AutoDedupReport {
    auto_dedup_inner(inputs, all_follow, lock, Some(only))
}

fn auto_dedup_inner(
    inputs: &[pins::Input],
    all_follow: &BTreeMap<String, String>,
    lock: &mut lock::LockFile,
    only: Option<&[String]>,
) -> AutoDedupReport {
    let aliases = AutoFollowAliases::from_inputs(inputs, all_follow);
    let mut valid = inputs
        .iter()
        .map(|i| i.name.clone())
        .collect::<HashSet<String>>();
    valid.extend(aliases.targets().cloned());
    let mut changed = prune_stale_auto_entries(lock, &valid);
    let comparator = CompareSession::new();

    if aliases.is_empty() {
        return AutoDedupReport {
            changed,
            scan_diagnostics: BTreeSet::new(),
            surfaced_fetch_causes: BTreeSet::new(),
        };
    }

    let scan_inputs = match only {
        Some(names) if !names.is_empty() => {
            inputs
                .iter()
                .filter(|inp| names.contains(&inp.name))
                .collect::<Vec<_>>()
        },
        _ => inputs.iter().collect::<Vec<_>>(),
    };
    let batches = scan_batches(&scan_inputs, &aliases, lock);
    let mut observations = BTreeMap::<String, Vec<LockObservation>>::new();
    let mut scan_diagnostics = BTreeSet::<ScanDiagnostic>::new();
    for batch in batches {
        scan_diagnostics.extend(batch.diagnostics);
        for (target, observation) in batch.observations {
            observations.entry(target).or_default().push(observation);
        }
    }

    for (target, mut obs) in observations {
        if let Some(current) = lock.get(&target) {
            obs.insert(0, LockObservation::from_node(current.clone()));
        }
        restrict_to_seed_identity(&mut obs);
        if let Some(winner) = LockObservation::choose(obs, |base, head| {
            comparator.compare_locked_nodes(base, head)
        }) && lock.get(&target) != Some(&winner)
        {
            lock.insert(target, winner);
            changed = true;
        }
    }

    AutoDedupReport {
        changed,
        scan_diagnostics,
        surfaced_fetch_causes: comparator.into_surfaced(),
    }
}

fn scan_batches(
    inputs: &[&pins::Input],
    aliases: &AutoFollowAliases,
    lock: &lock::LockFile,
) -> Vec<ScanBatch> {
    let scan_jobs = inputs
        .iter()
        .copied()
        .filter(|inp| inp.pin_type == PinType::Flake)
        .collect::<Vec<_>>();
    dispatcher::ordered(scan_jobs, AUTO_DEDUP_SCAN_IN_FLIGHT, |_, input| {
        scan_input(input, aliases, lock)
    })
    .into_iter()
    .flatten()
    .collect::<Vec<ScanBatch>>()
}

fn scan_input(
    input: &pins::Input,
    aliases: &AutoFollowAliases,
    lock: &lock::LockFile,
) -> Option<ScanBatch> {
    let node = lock.get(&input.name)?;
    let path = vec![input.name.clone()];
    let mut batch = ScanBatch::default();
    let (maybe_raw, maybe_cause) = tolerate(try_raw_file(node, ScanFile::FlakeLock));
    if let Some(cause) = maybe_cause {
        batch
            .diagnostics
            .insert(ScanDiagnostic::fetch(&path, ScanFile::FlakeLock, cause));
    }
    let Some(raw_body) = maybe_raw.flatten() else {
        return (!batch.diagnostics.is_empty()).then_some(batch);
    };
    let parsed = match lock::FlakeLock::parse(&raw_body) {
        Ok(parsed) => parsed,
        Err(err) => {
            batch
                .diagnostics
                .insert(ScanDiagnostic::parse(&path, ScanFile::FlakeLock, err));
            return Some(batch);
        },
    };
    for (key, locked) in parsed.locked_nodes() {
        let stripped = strip_disambiguator(key);
        let Some(target) = aliases.target_for(stripped) else {
            continue;
        };
        batch
            .observations
            .push((target.clone(), LockObservation::from_node(locked.clone())));
    }
    Some(batch)
}

/// keep only observations matching the seed in both source identity and fetch
/// kind, so auto-dedup never realigns a target across repositories or flips its
/// fetch mechanism (and thus its narHash).
fn restrict_to_seed_identity(observations: &mut Vec<LockObservation>) {
    let Some(seed) = observations.first() else {
        return;
    };
    let reference_id = SourceId::from_locked(&seed.node);
    let reference_kind = discriminant(&seed.node);
    observations.retain(|obs| {
        discriminant(&obs.node) == reference_kind
            && SourceId::from_locked(&obs.node) == reference_id
    });
}

fn prune_stale_auto_entries(lock: &mut lock::LockFile, valid: &HashSet<String>) -> bool {
    let stale = lock
        .keys()
        .filter(|key| !valid.contains(key.as_str()))
        .cloned()
        .collect::<Vec<String>>();
    for key in &stale {
        lock.remove(key);
    }
    !stale.is_empty()
}

#[cfg(test)]
mod tests {
    use super::{
        LockObservation,
        restrict_to_seed_identity,
    };
    use crate::{
        fetch::CompareStatus,
        lock::LockedNode,
    };

    fn github_node(rev: &str) -> LockedNode {
        LockedNode::new_github("o", "r", rev, "sha256-n", 0)
    }

    fn github_node_in(owner: &str, repo: &str, rev: &str) -> LockedNode {
        LockedNode::new_github(owner, repo, rev, "sha256-n", 0)
    }

    fn git_node(url: &str, rev: &str) -> LockedNode {
        LockedNode::new_git(url, "main", rev, "sha256-n", 0, false)
    }

    fn node_rev(node: &LockedNode) -> &str {
        node.rev().unwrap()
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
                    ("base", "ahead") => Some(CompareStatus::Ahead),
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
                    ("base", "behind") => Some(CompareStatus::Behind),
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
                    ("base", "amended") => Some(CompareStatus::Diverged),
                    _ => None,
                }
            },
        )
        .unwrap();

        assert_eq!(node_rev(&winner), "amended");
    }

    #[test]
    fn restrict_to_seed_identity_drops_foreign_repo_despite_newer_timestamp() {
        let mut obs = vec![
            LockObservation::new(100, github_node_in("o", "r", "current")),
            LockObservation::new(900, github_node_in("fork", "r", "foreign")),
            LockObservation::new(800, github_node_in("o", "r", "sibling")),
        ];
        restrict_to_seed_identity(&mut obs);

        let revs = obs
            .iter()
            .map(|entry| node_rev(&entry.node))
            .collect::<Vec<_>>();
        assert_eq!(revs, vec!["current", "sibling"]);
    }

    #[test]
    fn restrict_to_seed_identity_drops_a_mismatched_fetch_kind() {
        let mut obs = vec![
            LockObservation::new(100, github_node_in("o", "r", "archive")),
            LockObservation::new(900, git_node("https://github.com/o/r.git", "checkout")),
        ];
        restrict_to_seed_identity(&mut obs);

        let revs = obs
            .iter()
            .map(|entry| node_rev(&entry.node))
            .collect::<Vec<_>>();
        assert_eq!(revs, vec!["archive"]);
    }
}
