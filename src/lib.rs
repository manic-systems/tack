// SPDX-License-Identifier: EUPL-1.2

mod api;
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

// this is our public surface
pub use api::{
    CommandOutcome,
    CommandResultSet,
    CommandStatus,
    Tack,
};
pub use cli::Command;
use color_eyre::config::HookBuilder;
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
pub use history::{
    Row as HistoryRow,
    View as HistoryView,
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
    CollapsedFollow,
    DedupGroup,
    DedupReport,
    FollowMap,
    FollowSuggestions,
    LookOutcome,
    LookReport,
    Mark,
    NameSources,
    PinLook,
    PinUpdate,
    RevGroup,
    UpdateOutcome,
    UpdateReport,
};
pub use source::{
    Source,
    id::SourceId,
};

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

/// exit code buckets config 3 fetch 4 else 1
fn exit_code(report: &eyre::Report) -> ExitCode {
    for cause in report.chain() {
        if cause.downcast_ref::<ConfigError>().is_some() {
            return ExitCode::from(3);
        }
        if cause.downcast_ref::<fetch::FetchError>().is_some() {
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
