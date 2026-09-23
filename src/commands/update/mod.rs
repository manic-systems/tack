// SPDX-License-Identifier: EUPL-1.2

use std::sync::OnceLock;

use misstep::Result;

mod core;

pub use core::fetch_input;

use crate::{
    commands::Selection,
    error::user_bail,
    fetch::BranchComparison,
    pins,
    project::Project,
    render,
    report::{
        LookOutcome,
        LookReport,
        PullPatch,
        PullStatus,
        Signed,
        TagMove,
        UpdateOutcome,
        UpdateReport,
    },
    ui::{
        Display,
        PinStatus,
    },
};

const LOG_LIMIT: usize = 5;

fn updated_status(
    old: Option<&str>,
    new: &str,
    comparison: BranchComparison,
    signed: Option<&Signed>,
) -> PinStatus {
    let signed_by = signed.cloned();
    if let Some(path) = render::local_path_identity(new) {
        return PinStatus::Updated {
            old: "LOCAL".to_owned(),
            new: path.to_owned(),
            comparison,
            signed_by,
        };
    }
    PinStatus::Updated {
        old: old.map_or_else(|| "NEW".to_owned(), render::display_identity),
        new: render::display_identity(new),
        comparison,
        signed_by,
    }
}

fn tagged_status(moved: &TagMove) -> PinStatus {
    PinStatus::Updated {
        old:        moved.old.clone().unwrap_or_else(|| "NEW".to_owned()),
        new:        moved.new.clone(),
        comparison: moved.comparison,
        signed_by:  moved.signed_by.clone(),
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
                ref signed_by,
            } => updated_status(old.as_deref(), new, comparison, signed_by.as_ref()),
            UpdateOutcome::Tagged(ref moved) => tagged_status(moved),
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
            UpdateOutcome::Frozen => Self::Skipped("frozen".to_owned()),
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
            } => updated_status(old.as_deref(), new, comparison, None),
            LookOutcome::Tagged(ref moved) => tagged_status(moved),
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

    fn begin(&self, selected: &[&pins::Input]) {
        let rows = selected
            .iter()
            .map(|input| (input.name.clone(), input.group.clone()))
            .collect();
        let _ = self.0.set(Display::new(rows));
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
    fn begin(&self, selected: &[&pins::Input]) {
        self.spinner.begin(selected);
    }

    fn fetching(&self, index: usize) {
        self.spinner.step(index, PinStatus::Fetching { frame: 0 });
    }

    fn finished(&self, index: usize, outcome: &O) {
        self.spinner.step(index, (self.status)(outcome));
    }
}

pub fn update(project: &Project, selection: Selection<'_>, accept: bool) -> Result<UpdateReport> {
    core::update(project, selection, accept, &core::NoProgress)
}

pub fn update_cli(project: &Project, selection: Selection<'_>, accept: bool) -> Result<()> {
    let spinner = Spinner::new();
    let report = core::update(project, selection, accept, &SpinnerProgress {
        spinner: &spinner,
        status:  update_status,
    })?;
    if let Some(display) = spinner.into_display() {
        display.finish();
    } else {
        print_empty_selection(project, selection);
    }
    print_warnings(&report.warnings);
    if let Some(message) = report.user_error() {
        user_bail!("{message}");
    }
    Ok(())
}

pub fn look(project: &Project, selection: Selection<'_>, verbose: bool) -> Result<LookReport> {
    core::look(project, selection, verbose, &core::NoProgress)
}

pub fn look_cli(project: &Project, selection: Selection<'_>, verbose: bool) -> Result<()> {
    let spinner = Spinner::new();
    let report = core::look(project, selection, verbose, &SpinnerProgress {
        spinner: &spinner,
        status:  look_status,
    })?;
    if let Some(display) = spinner.into_display() {
        let logs = report
            .pins
            .iter()
            .map(|pin| pin.log.clone().filter(|_| verbose))
            .collect::<Vec<_>>();
        let notes = report
            .pins
            .iter()
            .map(|pin| {
                pin.pulls
                    .iter()
                    .map(|pull| pull_note(&pin.name, pull))
                    .collect()
            })
            .collect::<Vec<_>>();
        display.finish_with(&logs, &notes);
    } else {
        print_empty_selection(project, selection);
    }
    print_warnings(&report.warnings);
    Ok(())
}

fn pull_note(pin: &str, pull: &PullPatch) -> String {
    let reference = &pull.reference;
    let source = &pull.source;
    match pull.status {
        PullStatus::Landed => {
            format!(
                "{reference} is upstream now, drop it with `tack patch rm {pin} {}`",
                shell_word(source)
            )
        },
        PullStatus::Merged { checked: true } => {
            format!("{reference} merged, but {pin} doesn't have it yet")
        },
        PullStatus::Merged { checked: false } => {
            format!("{reference} merged, tack can't tell whether {pin} has it yet")
        },
        PullStatus::Closed => format!("{reference} was closed without merging"),
        PullStatus::Changed => {
            format!(
                "{reference} changed since you vendored it, `tack patch update {pin}` takes the \
                 new version"
            )
        },
    }
}

/// `word` quoted for a shell when it carries anything a shell would act on
fn shell_word(word: &str) -> String {
    let plain = word
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:@%+=,".contains(&byte));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// nothing ran, so say whether the project is empty or the selection emptied it
fn print_empty_selection(project: &Project, selection: Selection<'_>) {
    if selection.is_everything() {
        println!(
            "no pins in {}, add one with `tack add <name> <url>`",
            project.pins_path().display()
        );
    } else {
        println!("no pins selected");
    }
}

fn print_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("tack: {warning}");
    }
}
