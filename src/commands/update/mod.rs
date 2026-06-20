// SPDX-License-Identifier: EUPL-1.2

use std::sync::OnceLock;

use eyre::Result;

mod core;

pub use core::fetch_input;

use crate::{
    error::user_bail,
    fetch::BranchComparison,
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

impl From<&UpdateOutcome> for PinStatus {
    fn from(outcome: &UpdateOutcome) -> Self {
        match *outcome {
            UpdateOutcome::Unchanged => Self::NoChange,
            UpdateOutcome::Updated {
                ref old,
                ref new,
                comparison,
            } => updated_status(old.as_deref(), new, comparison),
            UpdateOutcome::Drift { ref rev, accepted } => {
                Self::Drift {
                    rev: render::short(rev),
                    accepted,
                }
            },
            UpdateOutcome::FixedDrift {
                ref old,
                ref new,
                accepted,
            } => {
                Self::FixedDrift {
                    old: render::short(old),
                    new: render::short(new),
                    accepted,
                }
            },
            UpdateOutcome::Failed(ref msg) => Self::Failed(msg.clone()),
        }
    }
}

impl From<&LookOutcome> for PinStatus {
    fn from(outcome: &LookOutcome) -> Self {
        match *outcome {
            LookOutcome::Unchanged => Self::NoChange,
            LookOutcome::Updated {
                ref old,
                ref new,
                comparison,
            } => updated_status(old.as_deref(), new, comparison),
            LookOutcome::Skipped(ref note) => Self::Skipped(note.clone()),
            LookOutcome::Failed(ref msg) => Self::Failed(msg.clone()),
        }
    }
}

fn update_status(outcome: &UpdateOutcome) -> PinStatus {
    outcome.into()
}

fn look_status(outcome: &LookOutcome) -> PinStatus {
    outcome.into()
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

struct SpinnerProgress<'a, O> {
    spinner: &'a Spinner,
    status:  fn(&O) -> PinStatus,
}

impl<O> core::Progress<O> for SpinnerProgress<'_, O> {
    fn begin(&self, names: &[String]) {
        self.spinner.begin(names);
    }

    fn fetching(&self, index: usize) {
        self.spinner.step(index, PinStatus::Fetching { frame: 0 });
    }

    fn finished(&self, index: usize, outcome: &O) {
        self.spinner.step(index, (self.status)(outcome));
    }
}

pub fn update(project: &Project, names: &[String], accept: bool) -> Result<UpdateReport> {
    core::update(project, names, accept, &core::NoProgress)
}

pub fn update_cli(project: &Project, names: &[String], accept: bool) -> Result<()> {
    let spinner = Spinner::new();
    let report = core::update(project, names, accept, &SpinnerProgress {
        spinner: &spinner,
        status:  update_status,
    })?;
    if let Some(display) = spinner.into_display() {
        display.finish();
    }
    print_warnings(&report.warnings);
    if let Some(message) = report.user_error() {
        user_bail!("{message}");
    }
    Ok(())
}

pub fn look(project: &Project, names: &[String], verbose: bool) -> Result<LookReport> {
    core::look(project, names, verbose, &core::NoProgress)
}

pub fn look_cli(project: &Project, names: &[String], verbose: bool) -> Result<()> {
    let spinner = Spinner::new();
    let report = core::look(project, names, verbose, &SpinnerProgress {
        spinner: &spinner,
        status:  look_status,
    })?;
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
