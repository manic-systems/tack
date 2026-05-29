// SPDX-License-Identifier: EUPL-1.2

mod cli;
mod commands;
mod fetch;
mod lock;
mod nar;
mod pins;
mod shorturl;
mod ui;

use std::process;

use cli::Command;

fn main() {
    if let Err(err) = run() {
        eprintln!("tack: {err:#}");
        process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let cmd = cli::parse()?;
    // every command except the resolver's own fixer (init) and help nags when
    // the resolver has drifted. do it after a successful command so it trails the
    // output and never piles onto an unrelated failure
    let check_resolver = !matches!(cmd, Command::Init { .. } | Command::Help);

    let res = match cmd {
        Command::Init {
            force,
            resolver,
            flake,
        } => commands::init(force, resolver, flake),
        Command::Update { names, accept } => commands::update(&names, accept),
        Command::Look { names, verbose } => commands::look(&names, verbose),
        Command::Add {
            name,
            url,
            pin_type,
            unpack,
            dir,
            submodules,
            follows,
        } => {
            commands::add(
                &name,
                &url,
                pin_type,
                unpack,
                dir.as_deref(),
                submodules,
                &follows,
            )
        },
        Command::Rm { name } => commands::rm(&name),
        Command::Alias { name, template, rm } => commands::alias(&name, template.as_deref(), rm),
        Command::Dedup => commands::dedup(),
        Command::Help => {
            commands::help();
            Ok(())
        },
    };

    if check_resolver && res.is_ok() {
        commands::warn_stale_resolver();
    }
    res
}
