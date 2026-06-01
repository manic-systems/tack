// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
        HashMap,
        HashSet,
    },
    iter,
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
use rayon::prelude::{
    IndexedParallelIterator as _,
    IntoParallelRefIterator as _,
    ParallelIterator as _,
};

use super::{
    dedup::{
        strip_disambiguator,
        try_raw_file,
    },
    select,
    tolerate,
};
use crate::{
    fetch::{
        self,
        github::{
            BranchComparison,
            CompareStatus,
        },
    },
    lock::{
        self,
        LockedNode,
    },
    pins::{
        self,
        PinType,
    },
    project::Project,
    render,
    source::Source,
    ui::{
        Display,
        PinStatus,
    },
};

struct UpdateFetch {
    node:       LockedNode,
    rev:        String,
    comparison: BranchComparison,
}

impl UpdateFetch {
    fn fetch(input: &pins::Input, expanded: &str, old_rev: Option<&str>) -> Result<Self> {
        match input.pin_type {
            PinType::Fixed => {
                fetch::fetch_fixed_pin(expanded, input.unpack).map(|(node, rev)| {
                    Self {
                        node,
                        rev,
                        comparison: BranchComparison::none(),
                    }
                })
            },
            PinType::Flake | PinType::Fetch => {
                let source = expanded.parse::<Source>()?;
                fetch::fetch_pin_compared(&source, input.submodules, old_rev).map(|fetched| {
                    Self {
                        node:       fetched.node,
                        rev:        fetched.rev,
                        comparison: fetched.comparison,
                    }
                })
            },
        }
    }
}

struct UpdateRunner<'a> {
    accept:  bool,
    display: &'a Display,
    drift:   &'a AtomicUsize,
}

impl<'a> UpdateRunner<'a> {
    const fn new(accept: bool, display: &'a Display, drift: &'a AtomicUsize) -> Self {
        Self {
            accept,
            display,
            drift,
        }
    }

    fn update_one(
        &self,
        index: usize,
        input: &pins::Input,
        expanded: &str,
        old: Option<&LockedNode>,
    ) -> Option<LockedNode> {
        self.display.set(index, PinStatus::Fetching { frame: 0 });
        let old_rev = old.and_then(LockedNode::rev);
        match UpdateFetch::fetch(input, expanded, old_rev) {
            // for fixed pins sha256 is the identity, so any mismatch is drift
            Ok(UpdateFetch { node, rev, .. })
                if input.pin_type == PinType::Fixed
                    && old_rev.is_some()
                    && old_rev != Some(rev.as_str()) =>
            {
                self.display.set(index, PinStatus::FixedDrift {
                    old:      old_rev.map(render::short).unwrap_or_default(),
                    new:      render::short(&rev),
                    accepted: self.accept,
                });
                self.accept_or_record_drift(node)
            },
            Ok(UpdateFetch { node, rev, .. }) if old_rev == Some(rev.as_str()) => {
                // same rev, if hash moved, upstream changed under a stable rev
                if Self::hash_drifted(old, &node) {
                    self.display.set(index, PinStatus::Drift {
                        rev:      render::short(&rev),
                        accepted: self.accept,
                    });
                    self.accept_or_record_drift(node)
                } else {
                    self.display.set(index, PinStatus::NoChange);
                    None
                }
            },
            Ok(UpdateFetch {
                node,
                rev,
                comparison,
            }) => {
                self.display.set(index, PinStatus::Updated {
                    old: old_rev.map_or_else(|| "NEW".into(), render::short),
                    new: render::short(&rev),
                    comparison,
                });
                Some(node)
            },
            Err(err) => {
                self.display
                    .set(index, PinStatus::Failed(format!("{err:#}")));
                None
            },
        }
    }

    fn accept_or_record_drift(&self, node: LockedNode) -> Option<LockedNode> {
        if self.accept {
            Some(node)
        } else {
            self.drift.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    fn hash_drifted(old: Option<&LockedNode>, node: &LockedNode) -> bool {
        matches!(
            (old.and_then(LockedNode::hash), node.hash()),
            (Some(prev), Some(curr)) if prev != curr
        )
    }
}

#[derive(Default)]
struct AutoDedupReport {
    changed:               bool,
    surfaced_fetch_causes: BTreeSet<String>,
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
pub(super) struct LockObservation {
    last_modified: i64,
    node:          LockedNode,
}

impl LockObservation {
    #[cfg(test)]
    pub(super) const fn new(last_modified: i64, node: LockedNode) -> Self {
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

    pub(super) fn choose<C>(observations: Vec<Self>, mut compare: C) -> Option<LockedNode>
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

struct GithubCompare {
    owner: String,
    repo:  String,
    base:  String,
    head:  String,
}

impl GithubCompare {
    fn from_nodes(base: &LockedNode, head: &LockedNode) -> Option<Self> {
        let (
            &LockedNode::Github {
                owner: ref base_owner,
                repo: ref base_repo,
                rev: ref base_rev,
                ..
            },
            &LockedNode::Github {
                owner: ref head_owner,
                repo: ref head_repo,
                rev: ref head_rev,
                ..
            },
        ) = (base, head)
        else {
            return None;
        };
        if !base_owner.eq_ignore_ascii_case(head_owner)
            || !base_repo.eq_ignore_ascii_case(head_repo)
        {
            return None;
        }
        Some(Self {
            owner: base_owner.to_owned(),
            repo:  base_repo.to_owned(),
            base:  base_rev.as_ref()?.to_owned(),
            head:  head_rev.as_ref()?.to_owned(),
        })
    }

    fn cache_key(&self) -> (String, String, String, String) {
        (
            self.owner.clone(),
            self.repo.clone(),
            self.base.clone(),
            self.head.clone(),
        )
    }
}

struct LockComparator {
    cache:    HashMap<(String, String, String, String), Option<CompareStatus>>,
    surfaced: BTreeSet<String>,
}

impl LockComparator {
    fn new() -> Self {
        Self {
            cache:    HashMap::new(),
            surfaced: BTreeSet::new(),
        }
    }

    fn compare_locked_nodes(
        &mut self,
        base: &LockedNode,
        head: &LockedNode,
    ) -> Option<CompareStatus> {
        let compare = GithubCompare::from_nodes(base, head)?;
        if compare.base == compare.head {
            return Some(CompareStatus::Identical);
        }
        let key = compare.cache_key();
        if let Some(cached) = self.cache.get(&key) {
            return *cached;
        }
        let (maybe_status, maybe_cause) = tolerate(fetch::github::compare_status(
            &key.0, &key.1, &key.2, &key.3,
        ));
        if let Some(cause) = maybe_cause {
            self.surfaced.insert(cause);
        }
        let status = maybe_status.flatten();
        self.cache.insert(key, status);
        status
    }

    fn into_surfaced(self) -> BTreeSet<String> {
        self.surfaced
    }
}

pub fn update(project: &Project, names: &[String], accept: bool) -> Result<()> {
    let doc = project.load_pins()?;
    let shorturls = doc.shorturls();
    let all = doc.inputs()?;
    let all_follow = doc.all_follows()?;
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(());
    }
    let mut lk = project.load_lock()?;

    let display = Display::new(selected.iter().map(|i| i.name.clone()).collect());
    let drift = AtomicUsize::new(0);
    let runner = UpdateRunner::new(accept, &display, &drift);

    let results = selected
        .par_iter()
        .enumerate()
        .map(|(i, inp)| {
            let expanded = shorturls.expand(&inp.url);
            let old = lk.get(&inp.name);
            runner.update_one(i, inp, &expanded, old)
        })
        .collect::<Vec<Option<LockedNode>>>();

    let mut changed = false;
    for (inp, result) in selected.iter().zip(results) {
        if let Some(node) = result {
            lk.insert(inp.name.clone(), node);
            changed = true;
        }
    }
    let drift_count = drift.load(Ordering::Relaxed);
    let auto_dedup = if drift_count == 0 {
        AutoDedupReport::write(&all, &all_follow, &mut lk)
    } else {
        AutoDedupReport::default()
    };
    if auto_dedup.changed {
        changed = true;
    }
    if changed {
        project.save_lock(&lk)?;
    }
    display.finish();
    for cause in auto_dedup.surfaced_fetch_causes {
        eprintln!("tack: {cause}");
    }

    if drift_count > 0 {
        bail!(
            "upstream content differs from lock (drifted pins kept; investigate, then re-run with \
             --accept to relock)"
        );
    }
    Ok(())
}

/// for every `[all_follow]` entry whose target is not a declared input, walk
/// all top-level flake.locks once and write the freshest transitive observation
/// by branch comparison when possible, falling back to `lastModified`
/// also prunes stale auto-dedup entries that no longer have a route
impl AutoDedupReport {
    fn write(
        inputs: &[pins::Input],
        all_follow: &BTreeMap<String, String>,
        lock: &mut lock::LockFile,
    ) -> Self {
        let aliases = AutoFollowAliases::from_inputs(inputs, all_follow);
        let mut valid = inputs
            .iter()
            .map(|i| i.name.clone())
            .collect::<HashSet<String>>();
        valid.extend(aliases.targets().cloned());
        let mut changed = Self::prune_stale_auto_entries(lock, &valid);
        let mut comparator = LockComparator::new();

        if aliases.is_empty() {
            return Self {
                changed,
                surfaced_fetch_causes: BTreeSet::new(),
            };
        }

        let probe_causes = Mutex::new(BTreeSet::<String>::new());
        let batches = {
            let lock_ro: &lock::LockFile = lock;
            inputs
                .par_iter()
                .filter(|inp| inp.pin_type == PinType::Flake)
                .filter_map(|inp| {
                    let node = lock_ro.get(&inp.name)?;
                    let (maybe_raw, maybe_cause) = tolerate(try_raw_file(node, "flake.lock"));
                    if let Some(cause) = maybe_cause {
                        probe_causes.lock().unwrap().insert(cause);
                    }
                    let raw_body = maybe_raw.flatten()?;
                    let parsed = lock::FlakeLock::parse(&raw_body).ok()?;
                    let mut local = Vec::<(String, LockObservation)>::new();
                    for (key, locked) in parsed.locked_nodes() {
                        let stripped = strip_disambiguator(key);
                        let Some(target) = aliases.target_for(stripped) else {
                            continue;
                        };
                        local.push((target.clone(), LockObservation::from_node(locked.clone())));
                    }
                    Some(local)
                })
                .collect::<Vec<Vec<(String, LockObservation)>>>()
        };
        comparator
            .surfaced
            .extend(probe_causes.into_inner().unwrap());

        let mut observations = BTreeMap::<String, Vec<LockObservation>>::new();
        for batch in batches {
            for (target, observation) in batch {
                observations.entry(target).or_default().push(observation);
            }
        }

        for (target, mut obs) in observations {
            if let Some(current) = lock.get(&target) {
                obs.insert(0, LockObservation::from_node(current.clone()));
            }
            if let Some(winner) = LockObservation::choose(obs, |base, head| {
                comparator.compare_locked_nodes(base, head)
            }) && lock.get(&target) != Some(&winner)
            {
                lock.insert(target, winner);
                changed = true;
            }
        }

        Self {
            changed,
            surfaced_fetch_causes: comparator.into_surfaced(),
        }
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
}

pub fn look(project: &Project, names: &[String], verbose: bool) -> Result<()> {
    const LOG_LIMIT: usize = 5;

    let doc = project.load_pins()?;
    let shorturls = doc.shorturls();
    let all = doc.inputs()?;
    if all.is_empty() {
        println!(
            "no pins in {}; add one with `tack add <name> <url>`",
            project.pins_path().display()
        );
        return Ok(());
    }
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(());
    }
    let lk = project.load_lock()?;

    let display = Display::new(selected.iter().map(|inp| inp.name.clone()).collect());
    let logs: Vec<Mutex<Option<fetch::github::CommitLog>>> = iter::repeat_with(|| Mutex::new(None))
        .take(selected.len())
        .collect();

    selected.par_iter().enumerate().for_each(|(i, inp)| {
        if inp.pin_type == PinType::Fixed {
            display.set(
                i,
                PinStatus::Skipped("fixed pin, run `tack update` to verify".into()),
            );
            return;
        }
        display.set(i, PinStatus::Fetching { frame: 0 });
        let expanded = shorturls.expand(&inp.url);
        let source = match expanded.parse::<Source>() {
            Ok(source) => source,
            Err(err) => {
                display.set(i, PinStatus::Failed(format!("{err:#}")));
                return;
            },
        };
        let old = lk
            .get(&inp.name)
            .and_then(LockedNode::rev)
            .map(str::to_owned);
        match fetch::current_rev_compared(&source, old.as_deref()) {
            Ok(current) if old.as_deref() == Some(current.rev.as_str()) => {
                display.set(i, PinStatus::NoChange);
            },
            Ok(current) => {
                display.set(i, PinStatus::Updated {
                    old:        old.as_deref().map_or_else(|| "NEW".into(), render::short),
                    new:        render::short(&current.rev),
                    comparison: current.comparison,
                });
                // commit logs are adjunct to the already-rendered rev status,
                // so keep them best-effort rather than surfacing fetch probes
                // while the spinner owns the display
                if verbose
                    && let Some(old_rev) = old.as_deref()
                    && let Ok(Some(log)) =
                        fetch::github::commits_between(&source, old_rev, &current.rev, LOG_LIMIT)
                {
                    *logs[i].lock().unwrap() = Some(log);
                }
            },
            Err(err) => display.set(i, PinStatus::Failed(format!("{err:#}"))),
        }
    });

    if verbose {
        let collected = logs
            .into_iter()
            .map(|mutex| mutex.into_inner().unwrap())
            .collect::<Vec<_>>();
        display.finish_verbose(&collected);
    } else {
        display.finish();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::LockObservation;
    use crate::{
        fetch::github::CompareStatus,
        lock::LockedNode,
    };

    fn github_node(rev: &str) -> LockedNode {
        LockedNode::new_github("o", "r", rev, "sha256-n", 0)
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
}
