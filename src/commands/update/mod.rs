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
        Dates,
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

/// a side of a move with its commit date, the way `nix flake update` prints it
fn dated(label: String, date: Option<u64>) -> String {
    match date.map(render::date) {
        Some(day) if !day.is_empty() => format!("{label} ({day})"),
        Some(_) | None => label,
    }
}

fn updated_status(
    old: Option<&str>,
    new: &str,
    comparison: BranchComparison,
    dates: Dates,
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
        old: dated(
            old.map_or_else(|| "NEW".to_owned(), render::display_identity),
            dates.old,
        ),
        new: dated(render::display_identity(new), dates.new),
        comparison,
        signed_by,
    }
}

fn tagged_status(moved: &TagMove) -> PinStatus {
    PinStatus::Updated {
        old:        dated(
            moved.old.clone().unwrap_or_else(|| "NEW".to_owned()),
            moved.dates.old,
        ),
        new:        dated(moved.new.clone(), moved.dates.new),
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
                dates,
                ref signed_by,
            } => updated_status(old.as_deref(), new, comparison, dates, signed_by.as_ref()),
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
            } => updated_status(old.as_deref(), new, comparison, Dates::default(), None),
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

pub fn update(
    project: &Project,
    selection: Selection<'_>,
    accept: bool,
    refresh_patches: bool,
) -> Result<UpdateReport> {
    core::update(
        project,
        selection,
        accept,
        refresh_patches,
        &core::NoProgress,
    )
}

pub fn update_cli(
    project: &Project,
    selection: Selection<'_>,
    accept: bool,
    refresh_patches: bool,
) -> Result<()> {
    let spinner = Spinner::new();
    let progress = SpinnerProgress {
        spinner: &spinner,
        status:  update_status,
    };
    let report = core::update(project, selection, accept, refresh_patches, &progress)?;
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
        let doc = project.load_pins()?;
        let notes = report
            .pins
            .iter()
            .map(|pin| {
                let moves = matches!(
                    pin.outcome,
                    LookOutcome::Updated { .. } | LookOutcome::Tagged(_)
                );
                let own = doc.own_patches(&pin.name);
                pin.pulls
                    .iter()
                    .map(|pull| pull_note(&pin.name, pull, moves.then_some(&*own)))
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

/// `own` holds the patches the pin lists itself when it has a newer rev for
/// `tack update` to take
fn pull_note(pin: &str, pull: &PullPatch, own: Option<&[String]>) -> String {
    let reference = &pull.reference;
    let source = &pull.source;
    match (&pull.status, own) {
        (&PullStatus::Landed, Some(listed)) if listed.contains(source) => {
            format!("{reference} is upstream now, `tack update {pin}` should drop it")
        },
        (&PullStatus::Landed, Some(_)) => {
            format!("{reference} is upstream now, remove it from the patches of {pin}'s group")
        },
        (&PullStatus::Landed, None) => {
            format!(
                "{reference} is upstream now, drop it with `tack patch rm {pin} {}`",
                render::shell_word(source)
            )
        },
        (&PullStatus::Merged { checked: true }, _) => {
            format!("{reference} merged, but {pin} doesn't have it yet")
        },
        (&PullStatus::Merged { checked: false }, _) => {
            format!("{reference} merged, tack can't tell whether {pin} has it yet")
        },
        (&PullStatus::Closed, _) => format!("{reference} was closed without merging"),
        (&PullStatus::Changed, Some(_)) => {
            format!(
                "{reference} changed since you vendored it, `tack update --refresh-patches {pin}` \
                 takes the new version with the new rev"
            )
        },
        (&PullStatus::Changed, None) => {
            format!(
                "{reference} changed since you vendored it, `tack patch update {pin}` takes the \
                 new version"
            )
        },
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
