// SPDX-License-Identifier: EUPL-1.2

use eyre::Result;

use super::LOG_LIMIT;
use crate::{
    commands::{
        dedup,
        select,
    },
    dispatcher,
    fetch::{
        self,
        BranchComparison,
        CompareStatus,
        FetchedPin,
        compare_planner::{
            CompareJob,
            CompareSession,
        },
        github::CommitLog,
    },
    lock::{
        LockIdentity,
        LockedNode,
    },
    pins::{
        self,
        PinType,
        Unpack,
    },
    project::Project,
    render,
    report::{
        LookOutcome,
        LookReport,
        PinLook,
        PinUpdate,
        UpdateOutcome,
        UpdateReport,
    },
    source::{
        self,
        Source,
    },
};

const UPDATE_IN_FLIGHT: usize = 16;
const LOOK_IN_FLIGHT: usize = 16;

pub(super) trait Progress<O>: Sync {
    fn begin(&self, names: &[String]);

    fn fetching(&self, index: usize);

    fn finished(&self, index: usize, outcome: &O);
}

pub(super) struct NoProgress;

impl<O> Progress<O> for NoProgress {
    fn begin(&self, _names: &[String]) {}

    fn fetching(&self, _index: usize) {}

    fn finished(&self, _index: usize, _outcome: &O) {}
}

pub fn fetch_input(
    pin_type: PinType,
    unpack: Option<Unpack>,
    submodules: bool,
    expanded: &str,
) -> Result<FetchedPin> {
    match pin_type {
        PinType::Fixed => fetch::fetch_fixed_pin(expanded, unpack),
        PinType::Flake | PinType::Fetch => {
            let source = expanded.parse::<Source>()?;
            fetch::fetch_pin(&source, submodules)
        },
    }
}

struct PinResolution {
    outcome: UpdateOutcome,
    node:    Option<LockedNode>,
    drift:   bool,
    warning: Option<String>,
}

fn classify(
    input: &pins::Input,
    expanded: &str,
    old: Option<&LockedNode>,
    accept: bool,
    warning: Option<String>,
    session: &CompareSession,
) -> PinResolution {
    let old_identity = old
        .and_then(LockedNode::resolved_identity)
        .map(LockIdentity::into_string);

    let source = expanded.parse::<Source>().ok();
    let resolved = if input.pin_type != PinType::Fixed
        && let Some(ref src) = source
    {
        session
            .resolve_and_compare(src, old_identity.as_deref())
            .ok()
    } else {
        None
    };

    if let Some(ref current) = resolved
        && old_identity.as_deref() == Some(current.rev.as_str())
    {
        return unchanged(warning);
    }

    let fetched = match fetch_input(input.pin_type, input.unpack, input.submodules, expanded) {
        Ok(fetched) => fetched,
        Err(err) => {
            return PinResolution {
                outcome: UpdateOutcome::Failed(format!("{err:#}")),
                node: None,
                drift: false,
                warning,
            };
        },
    };
    let (node, identity) = fetched.into_parts();
    let new_identity = String::from(identity);

    if old == Some(&node) {
        return unchanged(warning);
    }
    if input.pin_type == PinType::Fixed
        && old_identity.is_some()
        && old_identity.as_deref() != Some(new_identity.as_str())
    {
        return resolve_drift(
            UpdateOutcome::FixedDrift {
                old:      old_identity.unwrap_or_default(),
                new:      new_identity,
                accepted: accept,
            },
            node,
            accept,
            warning,
        );
    }
    if old_identity.as_deref() == Some(new_identity.as_str()) {
        return if hash_drifted(old, &node) {
            resolve_drift(
                UpdateOutcome::Drift {
                    rev:      new_identity,
                    accepted: accept,
                },
                node,
                accept,
                warning,
            )
        } else {
            unchanged(warning)
        };
    }

    let comparison = resolved
        .filter(|current| current.rev == new_identity)
        .map_or_else(
            || {
                source
                    .as_ref()
                    .map_or_else(BranchComparison::default, |src| {
                        compare_with_planner(session, src, old_identity.as_deref(), &new_identity)
                    })
            },
            |current| current.comparison,
        );

    PinResolution {
        outcome: UpdateOutcome::Updated {
            old: old_identity,
            new: new_identity,
            comparison,
        },
        node: Some(node),
        drift: false,
        warning,
    }
}

fn compare_with_planner(
    session: &CompareSession,
    source: &Source,
    old_rev: Option<&str>,
    new_rev: &str,
) -> BranchComparison {
    let Some(previous_rev) = old_rev else {
        return BranchComparison::default();
    };
    if previous_rev == new_rev {
        return BranchComparison::verified(CompareStatus::Identical);
    }
    let Some(job) = CompareJob::from_source(source, previous_rev, new_rev) else {
        return BranchComparison::default();
    };
    session
        .compare(&job)
        .status
        .map_or_else(BranchComparison::unavailable, BranchComparison::verified)
}

const fn unchanged(warning: Option<String>) -> PinResolution {
    PinResolution {
        outcome: UpdateOutcome::Unchanged,
        node: None,
        drift: false,
        warning,
    }
}

fn resolve_drift(
    outcome: UpdateOutcome,
    node: LockedNode,
    accept: bool,
    warning: Option<String>,
) -> PinResolution {
    if accept {
        PinResolution {
            outcome,
            node: Some(node),
            drift: false,
            warning,
        }
    } else {
        PinResolution {
            outcome,
            node: None,
            drift: true,
            warning,
        }
    }
}

fn hash_drifted(old: Option<&LockedNode>, node: &LockedNode) -> bool {
    matches!(
        (old.and_then(LockedNode::hash), node.hash()),
        (Some(prev), Some(curr)) if prev != curr
    )
}

fn pin_names(selected: &[&pins::Input]) -> Vec<String> {
    selected.iter().map(|input| input.name.clone()).collect()
}

pub(super) fn update(
    project: &Project,
    names: &[String],
    accept: bool,
    progress: &impl Progress<UpdateOutcome>,
) -> Result<UpdateReport> {
    let doc = project.load_pins()?;
    let shorturls = doc.shorturls();
    let all = doc.inputs()?;
    let all_follow = doc.all_follows()?;
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(UpdateReport::default());
    }
    let mut lock = project.load_lock()?;
    progress.begin(&pin_names(&selected));

    let session = CompareSession::new();
    let resolution_jobs = selected.clone();
    let resolutions = dispatcher::ordered(resolution_jobs, UPDATE_IN_FLIGHT, |index, input| {
        progress.fetching(index);
        let localized =
            source::localize_path_url_with_warning(&shorturls.expand(&input.url), project.dir());
        let old = lock.get(&input.name);
        let resolution = classify(
            input,
            &localized.url,
            old,
            accept,
            localized.warning,
            &session,
        );
        progress.finished(index, &resolution.outcome);
        resolution
    });

    let mut changed = false;
    let mut drift = 0_usize;
    let mut pins = Vec::with_capacity(resolutions.len());
    let mut warnings = Vec::new();
    let mut changed_names = Vec::new();
    for (input, resolution) in selected.iter().zip(resolutions) {
        if let Some(warning) = resolution.warning {
            warnings.push(warning);
        }
        if let Some(node) = resolution.node {
            lock.insert(input.name.clone(), node);
            changed = true;
            changed_names.push(input.name.clone());
        }
        if resolution.drift {
            drift += 1;
        }
        pins.push(PinUpdate {
            name:    input.name.clone(),
            outcome: resolution.outcome,
        });
    }

    let auto_dedup = if drift == 0 && !changed_names.is_empty() {
        dedup::auto_dedup_scoped(&all, &all_follow, &mut lock, &changed_names)
    } else {
        dedup::AutoDedupReport::default()
    };
    if auto_dedup.changed {
        changed = true;
    }
    if changed {
        project.save_lock(&lock)?;
    }

    for diagnostic in auto_dedup.scan_diagnostics {
        warnings.push(render::scan_diagnostic(&diagnostic));
    }
    warnings.extend(session.into_surfaced());
    warnings.extend(auto_dedup.surfaced_fetch_causes);
    warnings.extend(fetch::drain_fetch_warnings());

    Ok(UpdateReport {
        pins,
        drift,
        warnings,
    })
}

fn classify_look(
    input: &pins::Input,
    expanded: &str,
    old: Option<&str>,
    verbose: bool,
    session: &CompareSession,
) -> (LookOutcome, Option<CommitLog>) {
    if input.pin_type == PinType::Fixed {
        return (
            LookOutcome::Skipped("fixed pin, run `tack update` to verify".to_owned()),
            None,
        );
    }
    let source = match expanded.parse::<Source>() {
        Ok(source) => source,
        Err(err) => return (LookOutcome::Failed(format!("{err:#}")), None),
    };
    if matches!(source, Source::Path { .. }) {
        return (LookOutcome::Skipped("local path".to_owned()), None);
    }
    match session.resolve_and_compare(&source, old) {
        Ok(current) if old == Some(current.rev.as_str()) => (LookOutcome::Unchanged, None),
        Ok(current) => {
            let log = match (verbose, old) {
                (true, Some(old_rev)) => {
                    fetch::github::commits_between(&source, old_rev, &current.rev, LOG_LIMIT)
                        .ok()
                        .flatten()
                },
                _ => None,
            };
            (
                LookOutcome::Updated {
                    old:        old.map(str::to_owned),
                    new:        current.rev,
                    comparison: current.comparison,
                },
                log,
            )
        },
        Err(err) => (LookOutcome::Failed(format!("{err:#}")), None),
    }
}

pub(super) fn look(
    project: &Project,
    names: &[String],
    verbose: bool,
    progress: &impl Progress<LookOutcome>,
) -> Result<LookReport> {
    let doc = project.load_pins()?;
    let shorturls = doc.shorturls();
    let all = doc.inputs()?;
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(LookReport::default());
    }
    let lock = project.load_lock()?;
    progress.begin(&pin_names(&selected));

    let session = CompareSession::new();
    let look_jobs = selected.clone();
    let look_results = dispatcher::ordered(look_jobs, LOOK_IN_FLIGHT, |index, input| {
        progress.fetching(index);
        let localized =
            source::localize_path_url_with_warning(&shorturls.expand(&input.url), project.dir());
        let old = lock
            .get(&input.name)
            .and_then(LockedNode::resolved_identity)
            .map(LockIdentity::into_string);
        let (outcome, log) =
            classify_look(input, &localized.url, old.as_deref(), verbose, &session);
        progress.finished(index, &outcome);
        (
            PinLook {
                name: input.name.clone(),
                outcome,
                log,
            },
            localized.warning,
        )
    });

    let mut warnings = Vec::new();
    let pins = look_results
        .into_iter()
        .map(|(pin, maybe_warning)| {
            if let Some(message) = maybe_warning {
                warnings.push(message);
            }
            pin
        })
        .collect::<Vec<PinLook>>();
    warnings.extend(session.into_surfaced());
    warnings.extend(fetch::drain_fetch_warnings());

    Ok(LookReport { pins, warnings })
}
