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

pub use api::{
    CommandOutcome,
    CommandResultSet,
    CommandStatus,
    Tack,
};
pub use cli::Command;
pub use commands::{
    AddRequest,
    InitRequest,
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
    LockIdentity,
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
    let cmd = cli::parse();

    match app::run(cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if expected(&err) {
                eprintln!("tack: {err:#}");
            } else {
                print_report(&err);
            }
            exit_code(&err)
        },
    }
}

/// an expected failure rather than a tack bug, so it prints as one line
fn expected(report: &misstep::Report) -> bool {
    report.chain().any(|cause| {
        cause.downcast_ref::<error::UserError>().is_some()
            || cause.downcast_ref::<ConfigError>().is_some()
            || cause.downcast_ref::<fetch::FetchError>().is_some()
    })
}

fn exit_code(report: &misstep::Report) -> ExitCode {
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
    reason = "unexpected errors print the full report, which misstep renders through Debug"
)]
fn print_report(err: &misstep::Report) {
    eprintln!("{err:?}");
}
