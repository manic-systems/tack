// SPDX-License-Identifier: EUPL-1.2

use eyre::Result;
use rayon::prelude::{
    IndexedParallelIterator as _,
    IntoParallelRefIterator as _,
    ParallelIterator as _,
};

use super::LOG_LIMIT;
use crate::{
    commands::{
        dedup,
        select,
    },
    fetch::{
        self,
        BranchComparison,
        github::CommitLog,
    },
    lock::LockedNode,
    pins::{
        self,
        PinType,
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
) -> PinResolution {
    let old_rev = old.and_then(LockedNode::rev);
    let UpdateFetch {
        node,
        rev,
        comparison,
    } = match UpdateFetch::fetch(input, expanded, old_rev) {
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

    if old == Some(&node) {
        return unchanged(warning);
    }
    if input.pin_type == PinType::Fixed && old_rev.is_some() && old_rev != Some(rev.as_str()) {
        return resolve_drift(
            UpdateOutcome::FixedDrift {
                old:      old_rev.unwrap_or_default().to_owned(),
                new:      rev,
                accepted: accept,
            },
            node,
            accept,
            warning,
        );
    }
    if old_rev == Some(rev.as_str()) {
        return if hash_drifted(old, &node) {
            resolve_drift(
                UpdateOutcome::Drift {
                    rev,
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
    PinResolution {
        outcome: UpdateOutcome::Updated {
            old: old_rev.map(str::to_owned),
            new: rev,
            comparison,
        },
        node: Some(node),
        drift: false,
        warning,
    }
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

    let resolutions = selected
        .par_iter()
        .enumerate()
        .map(|(index, input)| {
            progress.fetching(index);
            let localized = source::localize_path_url_with_warning(
                &shorturls.expand(&input.url),
                project.dir(),
            );
            let old = lock.get(&input.name);
            let resolution = classify(input, &localized.url, old, accept, localized.warning);
            progress.finished(index, &resolution.outcome);
            resolution
        })
        .collect::<Vec<PinResolution>>();

    let mut changed = false;
    let mut drift = 0_usize;
    let mut pins = Vec::with_capacity(resolutions.len());
    let mut warnings = Vec::new();
    for (input, resolution) in selected.iter().zip(resolutions) {
        if let Some(warning) = resolution.warning {
            warnings.push(warning);
        }
        if let Some(node) = resolution.node {
            lock.insert(input.name.clone(), node);
            changed = true;
        }
        if resolution.drift {
            drift += 1;
        }
        pins.push(PinUpdate {
            name:    input.name.clone(),
            outcome: resolution.outcome,
        });
    }

    let auto_dedup = if drift == 0 {
        dedup::auto_dedup(&all, &all_follow, &mut lock)
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
    warnings.extend(auto_dedup.surfaced_fetch_causes);
    warnings.extend(fetch::http::drain_token_warnings());

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
    match fetch::current_rev_compared(&source, old) {
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

    let look_results = selected
        .par_iter()
        .enumerate()
        .map(|(index, input)| {
            progress.fetching(index);
            let localized = source::localize_path_url_with_warning(
                &shorturls.expand(&input.url),
                project.dir(),
            );
            let old = lock
                .get(&input.name)
                .and_then(LockedNode::rev)
                .map(str::to_owned);
            let (outcome, log) = classify_look(input, &localized.url, old.as_deref(), verbose);
            progress.finished(index, &outcome);
            (
                PinLook {
                    name: input.name.clone(),
                    outcome,
                    log,
                },
                localized.warning,
            )
        })
        .collect::<Vec<(PinLook, Option<String>)>>();

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
    warnings.extend(fetch::http::drain_token_warnings());

    Ok(LookReport { pins, warnings })
}
