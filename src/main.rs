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
    match cli::parse()? {
        Command::Init { force } => commands::init(force),
        Command::Update { names, accept } => commands::update(&names, accept),
        Command::Look { names } => commands::look(&names),
        Command::Add {
            name,
            url,
            flake,
            dir,
            submodules,
            follows,
        } => commands::add(&name, &url, flake, dir.as_deref(), submodules, &follows),
        Command::Rm { name } => commands::rm(&name),
        Command::Alias { name, template, rm } => commands::alias(&name, template.as_deref(), rm),
        Command::Help => {
            commands::help();
            Ok(())
        },
    }
}
