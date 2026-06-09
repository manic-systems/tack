// SPDX-License-Identifier: EUPL-1.2

mod app;
mod cli;
mod commands;
mod dispatcher;
mod error;
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

use std::process::ExitCode;

use color_eyre::config::HookBuilder;
// the curated public surface: the same operations the CLI runs, callable
// directly, plus the data types they read and write
pub use commands::{
    AddRequest,
    InitRequest,
    add,
    alias,
    dedup,
    init,
    look,
    redo,
    rm,
    undo,
    update,
};
pub use fetch::{
    BranchComparison,
    CompareStatus,
    github::CommitLog,
};
pub use lock::{
    LockFile,
    LockedNode,
};
pub use pins::{
    Input,
    PinType,
    PinsDoc,
    Unpack,
};
pub use project::{
    ConfigError,
    Project,
};
pub use report::{
    LookOutcome,
    LookReport,
    PinLook,
    PinUpdate,
    UpdateOutcome,
    UpdateReport,
};
pub use source::{
    Source,
    id::SourceId,
};

/// run tack as its CLI: install the error hook, parse argv, dispatch, and map
/// the outcome to a process exit code. this is the whole binary
#[must_use]
pub fn run() -> ExitCode {
    if let Err(err) = HookBuilder::default().display_env_section(false).install() {
        eprintln!("tack: {err}");
        return ExitCode::FAILURE;
    }

    let cmd = cli::parse();

    match app::run(cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if err
                .chain()
                .any(|cause| cause.downcast_ref::<error::UserError>().is_some())
            {
                eprintln!("tack: {err:#}");
            } else {
                print_report(&err);
            }
            exit_code(&err)
        },
    }
}

/// classify a failure into tack's exit codes: config (3), fetch (4), else 1
fn exit_code(report: &eyre::Report) -> ExitCode {
    for cause in report.chain() {
        if cause.downcast_ref::<ConfigError>().is_some() {
            return ExitCode::from(3);
        }
        if cause.downcast_ref::<fetch::http::FetchError>().is_some() {
            return ExitCode::from(4);
        }
    }
    ExitCode::FAILURE
}

#[expect(
    clippy::use_debug,
    reason = "color-eyre renders Report through its Debug implementation"
)]
fn print_report(err: &eyre::Report) {
    eprintln!("{err:?}");
}
