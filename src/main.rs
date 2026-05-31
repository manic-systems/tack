// SPDX-License-Identifier: EUPL-1.2

mod cli;
mod commands;
mod fetch;
mod history;
mod lock;
mod nar;
mod pins;
mod project;
mod render;
mod shorturl;
mod source;
mod ui;

use std::process::{
    ExitCode,
    Termination,
};

use cli::Command;
use color_eyre::config::HookBuilder;
use project::Project;

fn main() -> TackExit {
    if let Err(err) = HookBuilder::default().display_env_section(false).install() {
        eprintln!("tack: {err}");
        return TackExit::Other;
    }

    let cmd = match cli::parse() {
        Ok(cmd) => cmd,
        Err(err) => {
            print_report(&err);
            return TackExit::Usage;
        },
    };

    match run(cmd) {
        Ok(()) => TackExit::Success,
        Err(err) => {
            print_report(&err);
            TackExit::from_report(&err)
        },
    }
}

fn run(cmd: Command) -> eyre::Result<()> {
    // every command except the resolver's own fixer (init) and help nags when
    // the resolver has drifted. do it after a successful command so it trails the
    // output and never piles onto an unrelated failure
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
        // mutating commands snapshot before/after and record the diff
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

/// run a mutating command, recording the resulting file diff to undo history.
/// records even on [`Err`], since a partial write is still recoverable.
fn recorded(label: &str, run: impl FnOnce() -> eyre::Result<()>) -> eyre::Result<()> {
    let project = Project::discover();
    let store = history::store_dir(&project);
    let pre = history::snapshot(&project);
    let res = run();
    let post = history::snapshot(&project);
    if history::record(&store, label, pre, post) {
        println!("captured external edit");
    }
    res
}

enum TackExit {
    Success,
    Usage,
    Config,
    Fetch,
    Other,
}

impl TackExit {
    fn from_report(report: &eyre::Report) -> Self {
        for cause in report.chain() {
            if cause.downcast_ref::<project::ConfigError>().is_some() {
                return Self::Config;
            }
            if cause.downcast_ref::<fetch::FetchError>().is_some() {
                return Self::Fetch;
            }
        }
        Self::Other
    }
}

impl Termination for TackExit {
    fn report(self) -> ExitCode {
        match self {
            Self::Success => ExitCode::SUCCESS,
            Self::Usage => ExitCode::from(2),
            Self::Config => ExitCode::from(3),
            Self::Fetch => ExitCode::from(4),
            Self::Other => ExitCode::FAILURE,
        }
    }
}

#[expect(
    clippy::use_debug,
    reason = "color-eyre renders Report through its Debug implementation"
)]
fn print_report(err: &eyre::Report) {
    eprintln!("{err:?}");
}
