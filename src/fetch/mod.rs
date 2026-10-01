// SPDX-License-Identifier: EUPL-1.2

use std::borrow::Cow;

use crate::source::Source;

mod archive;
mod auth;
pub mod compare_planner;
mod error;
pub mod forge;
mod git;
mod git_http;
pub mod github;
mod github_commits;
pub mod gitlab;
mod http;
mod resolve;
mod time;
mod topology;

pub use auth::drain_fetch_warnings;
pub use error::{
    FetchError,
    FetchResult,
};
pub use resolve::{
    FetchedPin,
    FetchedTree,
    channel_repo,
    channel_rev,
    commit_object,
    commit_range,
    fetch_fixed_pin,
    fetch_locked_scan_files,
    fetch_locked_tree_into,
    fetch_pin,
    fetch_tree_into,
    forge_miss_untrusted,
    forge_raw_file,
    has_token,
    is_local_url,
    list_tags,
    locked_file,
    raw,
    raw_file,
};
pub use topology::{
    BranchComparison,
    CommitLog,
    CommitObject,
    CommitRange,
    CompareStatus,
    CurrentRev,
};

const PERCENT_ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// [`None`] when the source's host has no API tack can read a log from
pub fn commits_between(
    source: &Source,
    old: &str,
    new: &str,
    limit: usize,
) -> FetchResult<Option<CommitLog>> {
    match *source {
        Source::Github {
            ref owner,
            ref repo,
            ..
        } => github::commits_between(owner, repo, old, new, limit).map(Some),
        Source::Git { ref url, .. } => {
            forge::detect_git_url(url)
                .map_or(Ok(None), |repo| forge::commit_log(&repo, old, new, limit))
        },
        Source::Gitlab { .. } | Source::Tarball { .. } | Source::Path { .. } => Ok(None),
    }
}

pub fn percent_encode(value: &str) -> Cow<'_, str> {
    percent_encoding::percent_encode(value.as_bytes(), PERCENT_ENCODE_SET).into()
}
