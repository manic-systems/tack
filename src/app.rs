// SPDX-License-Identifier: EUPL-1.2

use crate::{
    cli::Command,
    commands,
    history::HistoryStore,
    project::Project,
};

pub fn run(cmd: Command) -> eyre::Result<()> {
    // every command except the resolver's own fixer (init) and help nags when
    // the resolver has drifted. do it after a successful command so it trails
    // the output and never piles onto an unrelated failure.
    let check_resolver = !matches!(cmd, Command::Init { .. } | Command::Help);

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
            recorded(label, move || commands::init(force, resolver, flake))
        },
        Command::Look { names, verbose } => commands::look(&names, verbose),
        Command::Dedup => commands::dedup(),
        Command::Undo { list } => commands::undo(list),
        Command::Redo => commands::redo(),
        Command::Help => {
            commands::help();
            Ok(())
        },
        Command::Update { names, accept } => {
            let label = if names.is_empty() {
                "update".to_owned()
            } else {
                format!("update {}", names.join(" "))
            };
            recorded(&label, move || commands::update(&names, accept))
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
            recorded(&label, move || {
                commands::add(
                    &name,
                    &url,
                    pin_type,
                    unpack,
                    dir.as_deref(),
                    submodules,
                    &follows,
                )
            })
        },
        Command::Rm { name } => {
            let label = format!("rm {name}");
            recorded(&label, move || commands::rm(&name))
        },
        Command::Alias { name, template, rm } => {
            let label = if rm {
                format!("alias --rm {name}")
            } else {
                format!("alias {name}")
            };
            recorded(&label, move || {
                commands::alias(&name, template.as_deref(), rm)
            })
        },
    };

    if check_resolver && res.is_ok() {
        commands::warn_stale_resolver();
    }
    res
}

fn recorded(label: &str, run: impl FnOnce() -> eyre::Result<()>) -> eyre::Result<()> {
    let project = Project::discover();
    let outcome = HistoryStore::for_project(&project).record_run(&project, label, run);
    if outcome.captured_external {
        println!("captured external edit");
    }
    outcome.result
}
