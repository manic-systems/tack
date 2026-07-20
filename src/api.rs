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

impl<'a> Tack<'a> {
    pub const fn new(project: &'a Project) -> Self {
        Self { project }
    }

    pub const fn project(&self) -> &'a Project {
        self.project
    }

    pub fn run(&self, cmd: Command) -> Result<CommandResultSet> {
        let check_resolver = !matches!(cmd, Command::Init { .. });
        let mut captured_external = false;
        let mut history_error = None;

        let label = cmd.history_label();
        let outcome = match cmd {
            Command::Init {
                force,
                resolver,
                flake,
                convert,
            } => {
                let recorded = self.recorded(&label, || {
                    commands::init(self.project, commands::InitRequest {
                        force,
                        resolver,
                        flake,
                        convert,
                    })
                })?;
                captured_external = recorded.captured_external;
                history_error = recorded.history_error;
                CommandOutcome::Init
            },
            Command::Update { names, accept } => {
                let recorded =
                    self.recorded(&label, || commands::update(self.project, &names, accept))?;
                captured_external = recorded.captured_external;
                history_error = recorded.history_error;
                CommandOutcome::Update(recorded.value)
            },
            Command::Look { names, verbose } => {
                CommandOutcome::Look(commands::look(self.project, &names, verbose)?)
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
                let recorded = self.recorded(&label, || {
                    commands::add(self.project, commands::AddRequest {
                        name: &name,
                        url: &url,
                        pin_type,
                        unpack,
                        dir: dir.as_deref(),
                        submodules,
                        follows: &follows,
                    })
                })?;
                captured_external = recorded.captured_external;
                history_error = recorded.history_error;
                CommandOutcome::Add
            },
            Command::Rm { name, prune } => {
                let recorded = self.recorded(&label, || {
                    commands::rm(self.project, name.as_deref(), prune)
                })?;
                captured_external = recorded.captured_external;
                history_error = recorded.history_error;
                CommandOutcome::Rm
            },
            Command::Alias { name, template, rm } => {
                let recorded = self.recorded(&label, || {
                    commands::alias(self.project, &name, template.as_deref(), rm)
                })?;
                captured_external = recorded.captured_external;
                history_error = recorded.history_error;
                CommandOutcome::Alias
            },
            Command::Dedup => CommandOutcome::Dedup(commands::dedup_report(self.project)?),
            Command::Undo { list } => {
                if list {
                    CommandOutcome::History(commands::history(self.project))
                } else {
                    CommandOutcome::Undo(commands::undo_view(self.project)?)
                }
            },
            Command::Redo => CommandOutcome::Redo(commands::redo_view(self.project)?),
        };

        let status = status_for(&outcome);
        let stale_resolver =
            check_resolver && status.is_success() && commands::stale_resolver(self.project);

        Ok(CommandResultSet {
            outcome,
            status,
            captured_external,
            history_error,
            stale_resolver,
        })
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
