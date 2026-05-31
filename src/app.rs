// SPDX-License-Identifier: EUPL-1.2

use crate::{
    cli::Command,
    commands,
    history::HistoryStore,
    project::Project,
};

pub fn run(cmd: Command) -> eyre::Result<()> {
    if matches!(cmd, Command::Help) {
        commands::help();
        return Ok(());
    }

    let project = Project::discover()?;

    // every command except init and help nags when the resolver drifts.
    // run after successful commands so it trails the output and never piles onto an
    // unrelated failure
    let check_resolver = !matches!(cmd, Command::Init { .. });

    let res = match cmd {
        Command::Init {
            force,
            resolver,
            flake,
        } => {
            let label = if resolver {
                "init --resolver"
            } else if flake {
                "init --flake"
            } else if force {
                "init --force"
            } else {
                "init"
            };
            recorded(&project, label, || {
                commands::init(&project, force, resolver, flake)
            })
        },
        Command::Look { names, verbose } => commands::look(&project, &names, verbose),
        Command::Dedup => commands::dedup(&project),
        Command::Undo { list } => commands::undo(&project, list),
        Command::Redo => commands::redo(&project),
        Command::Help => Ok(()),
        Command::Update { names, accept } => {
            let label = if names.is_empty() {
                "update".to_owned()
            } else {
                format!("update {}", names.join(" "))
            };
            recorded(&project, &label, || {
                commands::update(&project, &names, accept)
            })
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
            let label = format!("add {name}");
            recorded(&project, &label, || {
                commands::add(&project, commands::AddRequest {
                    name: &name,
                    url: &url,
                    pin_type,
                    unpack,
                    dir: dir.as_deref(),
                    submodules,
                    follows: &follows,
                })
            })
        },
        Command::Rm { name } => {
            let label = format!("rm {name}");
            recorded(&project, &label, || commands::rm(&project, &name))
        },
        Command::Alias { name, template, rm } => {
            let label = if rm {
                format!("alias --rm {name}")
            } else {
                format!("alias {name}")
            };
            recorded(&project, &label, || {
                commands::alias(&project, &name, template.as_deref(), rm)
            })
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
    run: impl FnOnce() -> eyre::Result<()>,
) -> eyre::Result<()> {
    let outcome = HistoryStore::for_project(project).record_run(project, label, run);
    if outcome.captured_external {
        println!("captured external edit");
    }
    outcome.result
}
