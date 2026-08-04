// SPDX-License-Identifier: EUPL-1.2

use eyre::Result;

use crate::{
    cli::Command,
    commands,
    history::{
        HistoryStore,
        Snapshot,
        View as HistoryView,
    },
    project::Project,
    report::{
        DedupReport,
        LookReport,
        UpdateReport,
    },
};

pub struct Tack<'a> {
    project: &'a Project,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandStatus {
    Success,
    UserError(String),
}

impl CommandStatus {
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }
}

#[derive(Debug)]
pub struct CommandResultSet {
    pub outcome:           CommandOutcome,
    pub status:            CommandStatus,
    pub captured_external: bool,
    pub history_error:     Option<String>,
    pub stale_resolver:    bool,
}

#[derive(Debug)]
pub enum CommandOutcome {
    Init,
    Update(UpdateReport),
    Look(LookReport),
    Add,
    Rm,
    Alias,
    Dedup(DedupReport),
    History(Option<HistoryView>),
    Undo(Option<HistoryView>),
    Redo(Option<HistoryView>),
}

struct Recorded<T> {
    value:             T,
    captured_external: bool,
    history_error:     Option<String>,
}

/// a read-only command touches neither pins.toml nor the lock, so there is
/// nothing to record
const fn unrecorded(value: CommandOutcome) -> Recorded<CommandOutcome> {
    Recorded {
        value,
        captured_external: false,
        history_error: None,
    }
}

impl<'a> Tack<'a> {
    pub const fn new(project: &'a Project) -> Self {
        Self { project }
    }

    pub const fn project(&self) -> &'a Project {
        self.project
    }

    pub fn run(&self, cmd: Command) -> Result<CommandResultSet> {
        let check_resolver = !matches!(cmd, Command::Init { .. });
        let label = cmd.history_label();
        let recorded = self.dispatch(cmd, &label)?;

        let status = status_for(&recorded.value);
        let stale_resolver =
            check_resolver && status.is_success() && commands::stale_resolver(self.project);

        Ok(CommandResultSet {
            outcome: recorded.value,
            status,
            captured_external: recorded.captured_external,
            history_error: recorded.history_error,
            stale_resolver,
        })
    }

    fn dispatch(&self, cmd: Command, label: &str) -> Result<Recorded<CommandOutcome>> {
        match cmd {
            Command::Init {
                force,
                resolver,
                flake,
                convert,
            } => {
                self.recorded(label, || {
                    commands::init(self.project, commands::InitRequest {
                        force,
                        resolver,
                        flake,
                        convert,
                    })
                    .map(|()| CommandOutcome::Init)
                })
            },
            Command::Update {
                exclude,
                names,
                accept,
            } => {
                let selection = commands::Selection {
                    names:   &names,
                    exclude: &exclude,
                };
                self.recorded(label, || {
                    commands::update(self.project, selection, accept).map(CommandOutcome::Update)
                })
            },
            Command::Look {
                exclude,
                names,
                verbose,
            } => {
                let selection = commands::Selection {
                    names:   &names,
                    exclude: &exclude,
                };
                let report = commands::look(self.project, selection, verbose)?;
                Ok(unrecorded(CommandOutcome::Look(report)))
            },
            Command::Add {
                name,
                url,
                pin_type,
                unpack,
                dir,
                submodules,
                follows,
            } => {
                self.recorded(label, || {
                    commands::add(self.project, commands::AddRequest {
                        name: &name,
                        url: &url,
                        pin_type,
                        unpack,
                        dir: dir.as_deref(),
                        submodules,
                        follows: &follows,
                    })
                    .map(|()| CommandOutcome::Add)
                })
            },
            Command::Rm { name } => {
                self.recorded(label, || {
                    commands::rm(self.project, &name).map(|()| CommandOutcome::Rm)
                })
            },
            Command::Alias { name, template, rm } => {
                self.recorded(label, || {
                    commands::alias(self.project, &name, template.as_deref(), rm)
                        .map(|()| CommandOutcome::Alias)
                })
            },
            Command::Dedup => {
                let report = commands::dedup_report(self.project)?;
                Ok(unrecorded(CommandOutcome::Dedup(report)))
            },
            Command::Undo { list } => {
                let outcome = if list {
                    CommandOutcome::History(commands::history(self.project))
                } else {
                    CommandOutcome::Undo(commands::undo_view(self.project)?)
                };
                Ok(unrecorded(outcome))
            },
            Command::Redo => {
                let view = commands::redo_view(self.project)?;
                Ok(unrecorded(CommandOutcome::Redo(view)))
            },
        }
    }

    fn recorded<T>(&self, label: &str, run: impl FnOnce() -> Result<T>) -> Result<Recorded<T>> {
        let pre = Snapshot::capture(self.project);
        let result = run();
        let post = Snapshot::capture(self.project);
        let recorded = HistoryStore::for_project(self.project).record(label, pre, post);
        let (captured_external, history_error) = match recorded {
            Ok(captured_external) => (captured_external, None),
            Err(err) => (false, Some(format!("{err:#}"))),
        };
        result.map(|value| {
            Recorded {
                value,
                captured_external,
                history_error,
            }
        })
    }
}

fn status_for(outcome: &CommandOutcome) -> CommandStatus {
    match *outcome {
        CommandOutcome::Update(ref report) => update_status(report),
        CommandOutcome::Init
        | CommandOutcome::Look(_)
        | CommandOutcome::Add
        | CommandOutcome::Rm
        | CommandOutcome::Alias
        | CommandOutcome::Dedup(_)
        | CommandOutcome::History(_)
        | CommandOutcome::Undo(_)
        | CommandOutcome::Redo(_) => CommandStatus::Success,
    }
}

fn update_status(report: &UpdateReport) -> CommandStatus {
    report
        .user_error()
        .map_or(CommandStatus::Success, CommandStatus::UserError)
}
