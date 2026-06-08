// SPDX-License-Identifier: EUPL-1.2

mod archive;
mod auth;
pub mod compare_planner;
mod error;
pub mod forge;
mod git;
mod git_http;
pub mod github;
pub mod gitlab;
mod http;
mod resolve;
mod time;
mod topology;

pub use auth::drain_token_warnings;
pub use error::{
    FetchError,
    FetchResult,
};
pub use resolve::{
    fetch_fixed_pin,
    fetch_locked_tree_into,
    fetch_pin,
    fetch_tree_into,
    raw,
};
pub use topology::{
    BranchComparison,
    CompareStatus,
    CurrentRev,
};

const PERCENT_ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');
