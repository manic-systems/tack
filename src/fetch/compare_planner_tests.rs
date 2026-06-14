// SPDX-License-Identifier: EUPL-1.2

use super::{
    CompareJob,
    CompareSource,
    dag_fallback_cause,
};
use crate::fetch::{
    CompareStatus,
    FetchError,
};

#[test]
fn identical_compare_job_is_verified_without_network() {
    let session = super::CompareSession::new();
    let attempt = session.compare(&CompareJob {
        source: CompareSource::ForgejoLike {
            host:  "git.example.com".to_owned(),
            owner: "o".to_owned(),
            repo:  "r".to_owned(),
        },
        base:   "same".to_owned(),
        head:   "same".to_owned(),
    });

    assert_eq!(attempt.status, Some(CompareStatus::Identical));
}

#[test]
fn dag_fallback_cause_surfaces_both_failures() {
    let api = FetchError::Github("rate limited".to_owned());
    let dag = FetchError::Transport("askpass: no tty".to_owned());
    let cause = dag_fallback_cause(Some(api), &dag);
    assert!(cause.contains("rate limited"), "api cause missing: {cause}");
    assert!(
        cause.contains("askpass: no tty"),
        "dag cause missing: {cause}"
    );
}
