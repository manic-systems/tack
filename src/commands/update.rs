// SPDX-License-Identifier: EUPL-1.2

use std::sync::OnceLock;

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
    dedup,
    select,
};
use crate::{
    fetch::{
        self,
        github::{
            BranchComparison,
            CommitLog,
        },
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
    ui::{
        Display,
        PinStatus,
    },
};

const LOG_LIMIT: usize = 5;

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

/// the per-pin verdict: what to show, whether to write the lock, and whether it
/// counts as kept drift
struct PinResolution {
    outcome: UpdateOutcome,
    node:    Option<LockedNode>,
    drift:   bool,
}

/// classify one pin against its lock entry, fetching as needed. pure, so the
/// silent and spinner paths share it
fn classify(
    input: &pins::Input,
    expanded: &str,
    old: Option<&LockedNode>,
    accept: bool,
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
                node:    None,
                drift:   false,
            };
        },
    };

    // nothing moved: the fetched node is byte-identical to the lock (the steady
    // state for a path pin, which has no upstream rev)
    if old == Some(&node) {
        return unchanged();
    }
    // for fixed pins sha256 is the identity; any mismatch is drift
    if input.pin_type == PinType::Fixed && old_rev.is_some() && old_rev != Some(rev.as_str()) {
        return resolve_drift(
            UpdateOutcome::FixedDrift {
                old:      old_rev.unwrap_or_default().to_owned(),
                new:      rev,
                accepted: accept,
            },
            node,
            accept,
        );
    }
    // same rev but hash moved: upstream changed under a stable rev
    if old_rev == Some(rev.as_str()) {
        return if hash_drifted(old, &node) {
            resolve_drift(
                UpdateOutcome::Drift {
                    rev,
                    accepted: accept,
                },
                node,
                accept,
            )
        } else {
            unchanged()
        };
    }
    // a genuine relock
    PinResolution {
        outcome: UpdateOutcome::Updated {
            old: old_rev.map(str::to_owned),
            new: rev,
            comparison,
        },
        node:    Some(node),
        drift:   false,
    }
}

const fn unchanged() -> PinResolution {
    PinResolution {
        outcome: UpdateOutcome::Unchanged,
        node:    None,
        drift:   false,
    }
}

/// drift relocks only with `--accept`; otherwise the lock is kept and the pin
/// is tallied as drift
fn resolve_drift(outcome: UpdateOutcome, node: LockedNode, accept: bool) -> PinResolution {
    if accept {
        PinResolution {
            outcome,
            node: Some(node),
            drift: false,
        }
    } else {
        PinResolution {
            outcome,
            node: None,
            drift: true,
        }
    }
}

fn hash_drifted(old: Option<&LockedNode>, node: &LockedNode) -> bool {
    matches!(
        (old.and_then(LockedNode::hash), node.hash()),
        (Some(prev), Some(curr)) if prev != curr
    )
}

/// the spinner status for a relocked/changed pin, shared by update and look
fn updated_status(old: Option<&str>, new: &str, comparison: BranchComparison) -> PinStatus {
    PinStatus::Updated {
        old: old.map_or_else(|| "NEW".to_owned(), render::short),
        new: render::short(new),
        comparison,
    }
}

/// map an update outcome onto its live spinner status
fn update_status(outcome: &UpdateOutcome) -> PinStatus {
    match *outcome {
        UpdateOutcome::Unchanged => PinStatus::NoChange,
        UpdateOutcome::Updated {
            ref old,
            ref new,
            comparison,
        } => updated_status(old.as_deref(), new, comparison),
        UpdateOutcome::Drift { ref rev, accepted } => {
            PinStatus::Drift {
                rev: render::short(rev),
                accepted,
            }
        },
        UpdateOutcome::FixedDrift {
            ref old,
            ref new,
            accepted,
        } => {
            PinStatus::FixedDrift {
                old: render::short(old),
                new: render::short(new),
                accepted,
            }
        },
        UpdateOutcome::Failed(ref msg) => PinStatus::Failed(msg.clone()),
    }
}

/// map a look outcome onto its live spinner status
fn look_status(outcome: &LookOutcome) -> PinStatus {
    match *outcome {
        LookOutcome::Unchanged => PinStatus::NoChange,
        LookOutcome::Updated {
            ref old,
            ref new,
            comparison,
        } => updated_status(old.as_deref(), new, comparison),
        LookOutcome::Skipped(ref note) => PinStatus::Skipped(note.clone()),
        LookOutcome::Failed(ref msg) => PinStatus::Failed(msg.clone()),
    }
}

fn pin_names(selected: &[&pins::Input]) -> Vec<String> {
    selected.iter().map(|input| input.name.clone()).collect()
}

/// the live spinner the `_cli` wrappers drive; the library passes no-ops
/// instead. the display is built lazily on `begin`, since the pin names it
/// needs are only known after selection
struct Spinner(OnceLock<Display>);

impl Spinner {
    const fn new() -> Self {
        Self(OnceLock::new())
    }

    fn begin(&self, names: &[String]) {
        let _ = self.0.set(Display::new(names.to_vec()));
    }

    fn step(&self, index: usize, status: PinStatus) {
        if let Some(display) = self.0.get() {
            display.set(index, status);
        }
    }

    fn into_display(self) -> Option<Display> {
        self.0.into_inner()
    }
}

/// the shared update core: fetch every selected pin in parallel, relock what
/// changed, run auto-dedup, and report. the progress hooks let the binary drive
/// a spinner while the library passes no-ops
fn run_update(
    project: &Project,
    names: &[String],
    accept: bool,
    on_begin: &(dyn Fn(&[String]) + Sync),
    on_progress: &(dyn Fn(usize, Option<&UpdateOutcome>) + Sync),
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
    on_begin(&pin_names(&selected));

    let resolutions = selected
        .par_iter()
        .enumerate()
        .map(|(index, input)| {
            on_progress(index, None);
            let expanded = source::localize_path_url(&shorturls.expand(&input.url), project.dir());
            let old = lock.get(&input.name);
            let resolution = classify(input, &expanded, old, accept);
            on_progress(index, Some(&resolution.outcome));
            resolution
        })
        .collect::<Vec<PinResolution>>();

    let mut changed = false;
    let mut drift = 0_usize;
    let mut pins = Vec::with_capacity(resolutions.len());
    for (input, resolution) in selected.iter().zip(resolutions) {
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

    let mut warnings = Vec::new();
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

/// fetch and relock pins, returning a structured report and writing no output
pub fn update(project: &Project, names: &[String], accept: bool) -> Result<UpdateReport> {
    run_update(project, names, accept, &|_| {}, &|_, _| {})
}

/// the CLI update: drive the live spinner, surface warnings, fail on kept drift
pub fn update_cli(project: &Project, names: &[String], accept: bool) -> Result<()> {
    let spinner = Spinner::new();
    let report = run_update(
        project,
        names,
        accept,
        &|pin_names| spinner.begin(pin_names),
        &|index, outcome| {
            spinner.step(
                index,
                outcome.map_or(PinStatus::Fetching { frame: 0 }, update_status),
            );
        },
    )?;
    if let Some(display) = spinner.into_display() {
        display.finish();
    }
    for warning in &report.warnings {
        eprintln!("tack: {warning}");
    }
    if report.drift > 0 {
        bail!(
            "upstream content differs from lock (drifted pins kept; investigate, then re-run with \
             --accept to relock)"
        );
    }
    Ok(())
}

/// compare one pin to upstream without touching the lock; fetch the commit log
/// only for a verbose, changed, github pin
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
            // commit logs are adjunct to rev status; keep their failures quiet
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

/// the shared look core: compare each selected pin to upstream without writing
/// the lock, collecting verbose commit logs when asked
fn run_look(
    project: &Project,
    names: &[String],
    verbose: bool,
    on_begin: &(dyn Fn(&[String]) + Sync),
    on_progress: &(dyn Fn(usize, Option<&LookOutcome>) + Sync),
) -> Result<LookReport> {
    let doc = project.load_pins()?;
    let shorturls = doc.shorturls();
    let all = doc.inputs()?;
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(LookReport::default());
    }
    let lock = project.load_lock()?;
    on_begin(&pin_names(&selected));

    let pins = selected
        .par_iter()
        .enumerate()
        .map(|(index, input)| {
            on_progress(index, None);
            let expanded = source::localize_path_url(&shorturls.expand(&input.url), project.dir());
            let old = lock
                .get(&input.name)
                .and_then(LockedNode::rev)
                .map(str::to_owned);
            let (outcome, log) = classify_look(input, &expanded, old.as_deref(), verbose);
            on_progress(index, Some(&outcome));
            PinLook {
                name: input.name.clone(),
                outcome,
                log,
            }
        })
        .collect::<Vec<PinLook>>();

    Ok(LookReport {
        pins,
        warnings: fetch::http::drain_token_warnings(),
    })
}

/// compare pins to upstream, returning a structured report and writing no
/// output
pub fn look(project: &Project, names: &[String], verbose: bool) -> Result<LookReport> {
    run_look(project, names, verbose, &|_| {}, &|_, _| {})
}

/// the CLI look: drive the spinner (with verbose commit logs) and surface
/// warnings
pub fn look_cli(project: &Project, names: &[String], verbose: bool) -> Result<()> {
    let spinner = Spinner::new();
    let report = run_look(
        project,
        names,
        verbose,
        &|pin_names| spinner.begin(pin_names),
        &|index, outcome| {
            spinner.step(
                index,
                outcome.map_or(PinStatus::Fetching { frame: 0 }, look_status),
            );
        },
    )?;
    if let Some(display) = spinner.into_display() {
        if verbose {
            let logs = report
                .pins
                .iter()
                .map(|pin| pin.log.clone())
                .collect::<Vec<_>>();
            display.finish_verbose(&logs);
        } else {
            display.finish();
        }
    } else if names.is_empty() {
        // no pins selected and no filter given means an empty project
        println!(
            "no pins in {}; add one with `tack add <name> <url>`",
            project.pins_path().display()
        );
    }
    for warning in &report.warnings {
        eprintln!("tack: {warning}");
    }
    Ok(())
}
