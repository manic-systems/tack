// SPDX-License-Identifier: EUPL-1.2

use crate::{
    cli::{
        Command,
        SignerAction,
    },
    commands,
    history::HistoryStore,
    project::Project,
};

pub fn run(cmd: Command) -> misstep::Result<()> {
    let scaffolding = matches!(cmd, Command::Init { .. });
    let project = if scaffolding {
        Project::here()?
    } else {
        Project::discover()?
    };

    // resolver nag trails successful output
    let check_resolver = !scaffolding;

    let label = cmd.history_label();
    let res = match cmd {
        Command::Init {
            force,
            resolver,
            flake,
            convert,
        } => {
            recorded(&project, &label, || {
                commands::init(&project, commands::InitRequest {
                    force,
                    resolver,
                    flake,
                    convert,
                })
            })
        },
        Command::Look {
            exclude,
            names,
            verbose,
        } => {
            commands::look_cli(
                &project,
                commands::Selection {
                    names:   &names,
                    exclude: &exclude,
                },
                verbose,
            )
        },
        Command::Tree { exclude, names } => {
            commands::tree_cli(&project, commands::Selection {
                names:   &names,
                exclude: &exclude,
            })
        },
        Command::Verify { base } => commands::verify_cli(&project, base.as_deref()),
        Command::Dedup => commands::dedup(&project),
        Command::Undo { list } => commands::undo(&project, list),
        Command::Redo => commands::redo(&project),
        Command::Update {
            exclude,
            names,
            accept,
        } => {
            recorded(&project, &label, || {
                commands::update_cli(
                    &project,
                    commands::Selection {
                        names:   &names,
                        exclude: &exclude,
                    },
                    accept,
                )
            })
        },
        Command::Add(args) => recorded(&project, &label, || commands::add(&project, &args)),
        Command::Rm { name } => recorded(&project, &label, || commands::rm(&project, &name)),
        Command::Alias { name, template, rm } => {
            recorded(&project, &label, || {
                commands::alias(&project, &name, template.as_deref(), rm)
            })
        },
        Command::SetFrozen { names, frozen } => {
            recorded(&project, &label, || {
                commands::set_frozen(&project, &names, frozen)
            })
        },
        Command::Signer(SignerAction::List) => commands::signer(&project, &SignerAction::List),
        Command::Signer(action) => {
            recorded(&project, &label, || commands::signer(&project, &action))
        },
        Command::Patch(action) => recorded(&project, &label, || commands::patch(&project, &action)),
        Command::Materialize { names } => {
            recorded(&project, &label, || commands::materialize(&project, &names))
        },
    };

    if check_resolver && res.is_ok() {
        commands::warn_stale_resolver(&project);
    }
    res
}

fn recorded(
    project: &Project,
    label: &str,
    run: impl FnOnce() -> misstep::Result<()>,
) -> misstep::Result<()> {
    let outcome = HistoryStore::for_project(project).record_run(project, label, run);
    if outcome.captured_external {
        println!("captured external edit");
    }
    if let Some(err) = outcome.history_error {
        eprintln!("tack: failed to record undo history: {err:#}");
    }
    outcome.result
}
