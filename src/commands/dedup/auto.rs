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
    FlakeVisit,
    FollowPolicy,
    OmitPolicy,
    walk_flake_lock,
};
use crate::{
    commands::tolerate,
    dispatcher,
    fetch::{
        self,
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
    render::printable,
    scan_diagnostic::{
        ScanDiagnostic,
        ScanFile,
    },
    source::{
        forge::Forge,
        id::SourceId,
    },
};

const AUTO_DEDUP_SCAN_IN_FLIGHT: usize = 16;

#[derive(Default)]
pub struct AutoDedupReport {
    pub changed:               bool,
    pub scan_diagnostics:      BTreeSet<ScanDiagnostic>,
    pub surfaced_fetch_causes: BTreeSet<String>,
    pub unseeded:              Vec<String>,
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
                .filter(|&(_, target)| {
                    !input_names.contains(target.as_str()) && !target.contains('/')
                })
                .filter_map(|(alias, target)| {
                    Some((
                        pins::rule_name(alias, pins::Side::Flake)?.to_owned(),
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

    /// aliases for targets not in the lock yet
    fn missing(&self, lock: &lock::LockFile) -> Self {
        Self {
            targets_by_alias: self
                .targets_by_alias
                .iter()
                .filter(|&(_, target)| {
                    lock.get(target).is_none() && !lock.unknown_nodes().any(|name| name == target)
                })
                .map(|(alias, target)| (alias.clone(), target.clone()))
                .collect(),
        }
    }

    fn unobserved<T>(&self, observed: &BTreeMap<String, T>) -> Vec<String> {
        let mut unobserved = BTreeMap::<&str, Vec<String>>::new();
        for (alias, target) in &self.targets_by_alias {
            if !observed.contains_key(target) {
                unobserved
                    .entry(target)
                    .or_default()
                    .push(format!("'{}'", printable(alias)));
            }
        }
        unobserved
            .into_iter()
            .map(|(target, aliases)| {
                format!(
                    "follow target '{}' was not locked, no flake pin locks an input named {}",
                    printable(target),
                    aliases.join(" or "),
                )
            })
            .collect()
    }
}

/// synthetic `[all_follow]` targets that `tack update` hasn't locked yet
pub(super) fn pending_targets(
    inputs: &[pins::Input],
    all_follow: &BTreeMap<String, String>,
    lock: &lock::LockFile,
) -> BTreeSet<String> {
    AutoFollowAliases::from_inputs(inputs, all_follow)
        .missing(lock)
        .targets()
        .cloned()
        .collect()
}

#[derive(Clone)]
struct LockObservation {
    last_modified: i64,
    node:          LockedNode,
}

impl From<LockedNode> for LockObservation {
    fn from(node: LockedNode) -> Self {
        let last_modified = node
            .last_modified()
            .and_then(|lm| i64::try_from(lm).ok())
            .unwrap_or(0);
        Self {
            last_modified,
            node,
        }
    }
}

impl LockObservation {
    #[cfg(test)]
    const fn new(last_modified: i64, node: LockedNode) -> Self {
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
            ..AutoDedupReport::default()
        };
    }

    // seed missing targets from every pin, and only refresh locked ones from
    // pins that moved
    let missing = aliases.missing(lock);
    let scan_jobs = inputs
        .iter()
        .filter_map(|inp| {
            let refresh = only.is_none_or(|names| names.contains(&inp.name));
            if refresh {
                Some((inp, &aliases))
            } else {
                (!missing.is_empty()).then_some((inp, &missing))
            }
        })
        .collect::<Vec<_>>();
    let batches = scan_batches(scan_jobs, lock);
    let mut observations = BTreeMap::<String, Vec<LockObservation>>::new();
    let mut scan_diagnostics = BTreeSet::<ScanDiagnostic>::new();
    for batch in batches {
        scan_diagnostics.extend(batch.diagnostics);
        for (target, observation) in batch.observations {
            observations.entry(target).or_default().push(observation);
        }
    }

    let unseeded = if scan_diagnostics.is_empty() {
        missing.unobserved(&observations)
    } else {
        Vec::new()
    };

    for (target, mut obs) in observations {
        if lock.patched(&target).is_some() || lock.unknown_nodes().any(|name| name == target) {
            continue;
        }
        if let Some(current) = lock.get(&target) {
            obs.insert(0, LockObservation::from(current.clone()));
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
        unseeded,
    }
}

fn scan_batches(
    jobs: Vec<(&pins::Input, &AutoFollowAliases)>,
    lock: &lock::LockFile,
) -> Vec<ScanBatch> {
    let scan_jobs = jobs
        .into_iter()
        .filter(|&(inp, _)| inp.pin_type == PinType::Flake)
        .collect::<Vec<_>>();
    dispatcher::ordered(
        scan_jobs,
        AUTO_DEDUP_SCAN_IN_FLIGHT,
        |_, (input, aliases)| scan_input(input, aliases, lock),
    )
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
    let (mut maybe_raw, maybe_cause) =
        tolerate(fetch::raw_file(node, ScanFile::FlakeLock.as_path()));
    if let Some(cause) = maybe_cause {
        batch
            .diagnostics
            .insert(ScanDiagnostic::fetch(&path, ScanFile::FlakeLock, cause));
    } else if maybe_raw.is_none()
        && let Some(forge) = Forge::from_locked(node)
        && fetch::forge_miss_untrusted(&forge)
    {
        let sparse = fetch::fetch_locked_scan_files(node, &[ScanFile::FlakeLock.as_path()])
            .ok()
            .flatten()
            .and_then(|files| files.into_iter().next());
        if sparse.is_some() {
            maybe_raw = sparse;
        } else {
            batch.diagnostics.insert(ScanDiagnostic::private_repo(
                &path,
                ScanFile::FlakeLock,
                forge.base(),
            ));
        }
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
    let Some(root) = parsed.root_node() else {
        batch.diagnostics.insert(ScanDiagnostic::parse(
            &path,
            ScanFile::FlakeLock,
            format!("flake lock root node '{}' does not exist", parsed.root()),
        ));
        return Some(batch);
    };
    // an omitted input can still be a follow target the resolver needs locked
    let (omit, follow) = (OmitPolicy::default(), FollowPolicy::default());
    walk_flake_lock(&parsed, root, &path, &omit, &follow, |visit| {
        match visit {
            FlakeVisit::Locked { name, locked, .. } => {
                if let Some(target) = aliases.target_for(name) {
                    batch
                        .observations
                        .push((target.clone(), LockObservation::from(locked.clone())));
                }
            },
            FlakeVisit::Unresolved(err) => {
                batch
                    .diagnostics
                    .insert(ScanDiagnostic::parse(&path, ScanFile::FlakeLock, err));
            },
            FlakeVisit::Followed { .. } | FlakeVisit::TooDeep => {},
        }
    });
    Some(batch)
}

/// fetch method affects the nar
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
#[path = "auto_tests.rs"]
mod tests;
