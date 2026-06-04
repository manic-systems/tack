// SPDX-License-Identifier: EUPL-1.2

use std::sync::OnceLock;

use eyre::{
    Result,
    bail,
};

mod core;

use crate::{
    fetch::github::BranchComparison,
    project::Project,
    render,
    report::{
        LookOutcome,
        LookReport,
        UpdateOutcome,
        UpdateReport,
    },
    ui::{
        Display,
        PinStatus,
    },
};

const LOG_LIMIT: usize = 5;

fn updated_status(old: Option<&str>, new: &str, comparison: BranchComparison) -> PinStatus {
    PinStatus::Updated {
        old: old.map_or_else(|| "NEW".to_owned(), render::short),
        new: render::short(new),
        comparison,
    }
}

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

struct UpdateProgress<'a> {
    spinner: &'a Spinner,
}

impl core::Progress<UpdateOutcome> for UpdateProgress<'_> {
    fn begin(&self, names: &[String]) {
        self.spinner.begin(names);
    }

    fn fetching(&self, index: usize) {
        self.spinner.step(index, PinStatus::Fetching { frame: 0 });
    }

    fn finished(&self, index: usize, outcome: &UpdateOutcome) {
        self.spinner.step(index, update_status(outcome));
    }
}

struct LookProgress<'a> {
    spinner: &'a Spinner,
}

impl core::Progress<LookOutcome> for LookProgress<'_> {
    fn begin(&self, names: &[String]) {
        self.spinner.begin(names);
    }

    fn fetching(&self, index: usize) {
        self.spinner.step(index, PinStatus::Fetching { frame: 0 });
    }

    fn finished(&self, index: usize, outcome: &LookOutcome) {
        self.spinner.step(index, look_status(outcome));
    }
}

pub fn update(project: &Project, names: &[String], accept: bool) -> Result<UpdateReport> {
    core::update(project, names, accept, &core::NoProgress)
}

pub fn update_cli(project: &Project, names: &[String], accept: bool) -> Result<()> {
    let spinner = Spinner::new();
    let report = core::update(project, names, accept, &UpdateProgress {
        spinner: &spinner,
    })?;
    if let Some(display) = spinner.into_display() {
        display.finish();
    }
    print_warnings(&report.warnings);
    if report.drift > 0 {
        bail!(
            "upstream content differs from lock (drifted pins kept; investigate, then re-run with \
             --accept to relock)"
        );
    }
    Ok(())
}

pub fn look(project: &Project, names: &[String], verbose: bool) -> Result<LookReport> {
    core::look(project, names, verbose, &core::NoProgress)
}

pub fn look_cli(project: &Project, names: &[String], verbose: bool) -> Result<()> {
    let spinner = Spinner::new();
    let report = core::look(project, names, verbose, &LookProgress { spinner: &spinner })?;
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
        println!(
            "no pins in {}; add one with `tack add <name> <url>`",
            project.pins_path().display()
        );
    }
    print_warnings(&report.warnings);
    Ok(())
}

fn print_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("tack: {warning}");
    }
}
