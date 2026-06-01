// SPDX-License-Identifier: EUPL-1.2

mod app;
mod cli;
mod commands;
mod fetch;
mod history;
mod lock;
mod nar;
mod pins;
mod project;
mod render;
mod report;
mod scan_diagnostic;
mod shorturl;
mod source;
mod ui;

use std::process::{
    ExitCode,
    Termination,
};

use color_eyre::config::HookBuilder;

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

    match app::run(cmd) {
        Ok(()) => TackExit::Success,
        Err(err) => {
            print_report(&err);
            TackExit::from_report(&err)
        },
    }
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
            if cause.downcast_ref::<fetch::http::FetchError>().is_some() {
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
