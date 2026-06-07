// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::{
        BTreeSet,
        HashMap,
    },
    sync::{
        Mutex,
        PoisonError,
    },
};

use eyre::Result;

use super::{
    BranchComparison,
    CompareStatus,
    CurrentRev,
    forge::{
        self,
        ForgeKind,
    },
    git,
    github,
    gitlab,
    http::{
        FetchError,
        FetchResult,
    },
    resolve,
};
use crate::{
    dispatcher,
    lock::LockedNode,
    source::{
        Source,
        gitlab as source_gitlab,
        id::SourceId,
    },
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CompareSource {
    Github {
        owner:    String,
        repo:     String,
        ref_hint: Option<GithubRef>,
    },
    Gitlab {
        host:  String,
        owner: String,
        repo:  String,
    },
    ForgejoLike {
        kind:  ForgejoLikeKind,
        host:  String,
        owner: String,
        repo:  String,
    },
    Git {
        url: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ForgejoLikeKind {
    Forgejo,
    Gitea,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum GithubRef {
    DefaultBranch,
    Named(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CompareJob {
    pub source: CompareSource,
    pub base:   String,
    pub head:   String,
}

#[derive(Clone, Debug)]
pub struct CompareAttempt {
    pub status: Option<CompareStatus>,
    pub cause:  Option<String>,
}

#[derive(Default)]
struct ComparePlanner {
    cache:    HashMap<CompareJob, CompareAttempt>,
    surfaced: BTreeSet<String>,
}

#[derive(Default)]
pub struct CompareSession {
    planner: Mutex<ComparePlanner>,
}

impl ComparePlanner {
    fn compare_cached(&self, job: &CompareJob) -> Option<CompareAttempt> {
        self.cache.get(job).cloned()
    }

    fn record(&mut self, job: CompareJob, attempt: CompareAttempt) {
        self.record_cause(&attempt);
        self.cache.insert(job, attempt);
    }

    pub fn into_surfaced(self) -> BTreeSet<String> {
        self.surfaced
    }

    fn record_cause(&mut self, attempt: &CompareAttempt) {
        if let Some(cause) = attempt.cause.as_ref() {
            self.surfaced.insert(cause.clone());
        }
    }
}

impl CompareSession {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn compare(&self, job: &CompareJob) -> CompareAttempt {
        if job.base == job.head {
            return CompareAttempt::verified(CompareStatus::Identical);
        }

        let cached = {
            self.planner
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .compare_cached(job)
        };
        if let Some(attempt) = cached {
            return attempt;
        }

        let attempt = execute(job);
        let mut planner = self.planner.lock().unwrap_or_else(PoisonError::into_inner);
        planner.record(job.clone(), attempt.clone());
        attempt
    }

    pub fn compare_batch(
        &self,
        jobs: Vec<CompareJob>,
        limit: usize,
    ) -> Vec<Option<CompareAttempt>> {
        let result_count = jobs.len();
        let mut queued = Vec::new();
        let mut primary_for = HashMap::<CompareJob, usize>::new();
        let mut duplicate_of = Vec::<(usize, usize)>::new();

        for (index, job) in jobs.into_iter().enumerate() {
            if let Some(primary) = primary_for.get(&job).copied() {
                duplicate_of.push((index, primary));
            } else {
                primary_for.insert(job.clone(), index);
                queued.push((index, job));
            }
        }

        let mut completed = vec![None; result_count];
        dispatcher::stream(
            queued,
            limit,
            |_, (index, job)| (index, self.compare(&job)),
            |_, (index, attempt)| {
                completed[index] = Some(attempt);
            },
        );

        for (index, primary) in duplicate_of {
            let (previous, current) = completed.split_at_mut(index);
            current[0].clone_from(&previous[primary]);
        }
        completed
    }

    pub fn compare_locked_nodes(
        &self,
        base: &LockedNode,
        head: &LockedNode,
    ) -> Option<CompareStatus> {
        let job = CompareJob::from_locked(base, head)?;
        self.compare(&job).status
    }

    pub fn resolve_and_compare(&self, source: &Source, base: Option<&str>) -> Result<CurrentRev> {
        if let Source::Github {
            ref owner,
            ref repo,
            ref reff,
            rev: None,
        } = *source
            && let Some(previous) = base
            && let Ok((rev, status)) =
                github::resolve_ref_compare(owner, repo, reff.as_deref(), previous)
        {
            return Ok(CurrentRev {
                comparison: status
                    .map_or_else(BranchComparison::unavailable, BranchComparison::verified),
                rev,
            });
        }

        let rev = resolve::current_rev(source)?;
        let comparison = match base {
            None => BranchComparison::none(),
            Some(previous) if previous == rev => {
                BranchComparison::verified(CompareStatus::Identical)
            },
            Some(previous) => {
                CompareJob::from_source(source, previous, &rev).map_or_else(
                    BranchComparison::none,
                    |job| {
                        self.compare(&job)
                            .status
                            .map_or_else(BranchComparison::unavailable, BranchComparison::verified)
                    },
                )
            },
        };
        Ok(CurrentRev { rev, comparison })
    }

    pub fn into_surfaced(self) -> BTreeSet<String> {
        self.planner
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
            .into_surfaced()
    }
}

impl CompareAttempt {
    pub const fn verified(status: CompareStatus) -> Self {
        Self {
            status: Some(status),
            cause:  None,
        }
    }

    const fn unavailable(cause: Option<String>) -> Self {
        Self {
            status: None,
            cause,
        }
    }
}

impl CompareJob {
    pub fn from_source(source: &Source, base: &str, head: &str) -> Option<Self> {
        Some(Self {
            source: CompareSource::from_source(source)?,
            base:   base.to_owned(),
            head:   head.to_owned(),
        })
    }

    pub fn from_source_id(id: &SourceId, base: &str, head: &str) -> Option<Self> {
        Some(Self {
            source: CompareSource::from_source_id(id)?,
            base:   base.to_owned(),
            head:   head.to_owned(),
        })
    }

    pub fn from_locked(base: &LockedNode, head: &LockedNode) -> Option<Self> {
        let id = SourceId::from_locked(base)?;
        if SourceId::from_locked(head).as_ref() != Some(&id) {
            return None;
        }
        Self::from_source_id(&id, base.rev()?, head.rev()?)
    }
}

impl CompareSource {
    pub fn from_source(source: &Source) -> Option<Self> {
        match *source {
            Source::Github {
                ref owner,
                ref repo,
                ref reff,
                ..
            } => {
                Some(Self::Github {
                    owner:    owner.clone(),
                    repo:     repo.clone(),
                    ref_hint: Some(
                        reff.clone()
                            .map_or(GithubRef::DefaultBranch, GithubRef::Named),
                    ),
                })
            },
            Source::Gitlab {
                ref host,
                ref owner,
                ref repo,
                ..
            } => {
                Some(Self::Gitlab {
                    host:  host.clone(),
                    owner: owner.clone(),
                    repo:  repo.clone(),
                })
            },
            Source::Git { .. } => {
                let target = source.git_target()?;
                Some(Self::Git {
                    url: target.url.to_lowercase(),
                })
            },
            Source::Tarball { .. } | Source::Path { .. } => None,
        }
    }

    pub fn from_source_id(id: &SourceId) -> Option<Self> {
        match *id {
            SourceId::Github {
                ref owner,
                ref repo,
            } => {
                Some(Self::Github {
                    owner:    owner.clone(),
                    repo:     repo.clone(),
                    ref_hint: None,
                })
            },
            SourceId::Gitlab {
                ref host,
                ref owner,
                ref repo,
            } => {
                Some(Self::Gitlab {
                    host:  host.clone(),
                    owner: owner.clone(),
                    repo:  repo.clone(),
                })
            },
            SourceId::Git { ref url } => Some(Self::Git { url: url.clone() }),
            SourceId::Tarball { .. } | SourceId::Indirect { .. } | SourceId::Path { .. } => None,
        }
    }

    fn from_detected_repo(repo: forge::HostedRepo) -> Option<Self> {
        match repo.kind {
            ForgeKind::Gitlab => {
                Some(Self::Gitlab {
                    host:  repo.host,
                    owner: repo.owner,
                    repo:  repo.repo,
                })
            },
            ForgeKind::Forgejo | ForgeKind::Gitea => {
                Some(Self::ForgejoLike {
                    kind:  forgejo_like_kind(repo.kind)?,
                    host:  repo.host,
                    owner: repo.owner,
                    repo:  repo.repo,
                })
            },
            ForgeKind::Cgit | ForgeKind::Unknown => None,
        }
    }

    fn clone_url(&self) -> String {
        match *self {
            Self::Github {
                ref owner,
                ref repo,
                ..
            } => format!("https://github.com/{owner}/{repo}.git"),
            Self::Gitlab {
                ref host,
                ref owner,
                ref repo,
            } => source_gitlab::clone_url(host, owner, repo),
            Self::ForgejoLike {
                ref host,
                ref owner,
                ref repo,
                ..
            } => format!("https://{host}/{owner}/{repo}.git"),
            Self::Git { ref url } => url.clone(),
        }
    }
}

fn execute(job: &CompareJob) -> CompareAttempt {
    match compare_api(job) {
        Ok(Some(status)) => CompareAttempt::verified(status),
        Ok(None) => fallback_dag(job, None),
        Err(err) => fallback_dag(job, Some(err)),
    }
}

fn compare_api(job: &CompareJob) -> FetchResult<Option<CompareStatus>> {
    match job.source {
        CompareSource::Github {
            ref owner,
            ref repo,
            ref ref_hint,
        } => compare_github(owner, repo, ref_hint.as_ref(), &job.base, &job.head),
        CompareSource::Gitlab {
            ref host,
            ref owner,
            ref repo,
        } => gitlab::compare_status(host, owner, repo, &job.base, &job.head),
        CompareSource::ForgejoLike {
            kind,
            ref host,
            ref owner,
            ref repo,
        } => forge::compare_status(kind.into(), host, owner, repo, &job.base, &job.head),
        CompareSource::Git { ref url } => compare_detected_git_url(url, &job.base, &job.head),
    }
}

fn compare_github(
    owner: &str,
    repo: &str,
    ref_hint: Option<&GithubRef>,
    base: &str,
    head: &str,
) -> FetchResult<Option<CompareStatus>> {
    if let Some(status) = ref_hint.and_then(|hint| {
        let reff = match *hint {
            GithubRef::DefaultBranch => None,
            GithubRef::Named(ref reff) => Some(reff.as_str()),
        };
        github::compare_ref_status(owner, repo, reff, base, head)
            .ok()
            .flatten()
    }) {
        return Ok(Some(status));
    }
    github::compare_status(owner, repo, base, head)
}

fn compare_detected_git_url(
    url: &str,
    base: &str,
    head: &str,
) -> FetchResult<Option<CompareStatus>> {
    let Some(repo) = forge::detect_git_url(url) else {
        return Ok(None);
    };
    let Some(source) = CompareSource::from_detected_repo(repo) else {
        return Ok(None);
    };
    compare_api(&CompareJob {
        source,
        base: base.to_owned(),
        head: head.to_owned(),
    })
}

const fn forgejo_like_kind(kind: ForgeKind) -> Option<ForgejoLikeKind> {
    match kind {
        ForgeKind::Forgejo => Some(ForgejoLikeKind::Forgejo),
        ForgeKind::Gitea => Some(ForgejoLikeKind::Gitea),
        ForgeKind::Gitlab | ForgeKind::Cgit | ForgeKind::Unknown => None,
    }
}

fn fallback_dag(job: &CompareJob, api_error: Option<FetchError>) -> CompareAttempt {
    match git::compare_status(&job.source.clone_url(), &job.base, &job.head) {
        Ok(Some(status)) => CompareAttempt::verified(status),
        Ok(None) => CompareAttempt::unavailable(api_error.map(|err| err.to_string())),
        Err(err) => {
            let cause = api_error.unwrap_or(err).to_string();
            CompareAttempt::unavailable(Some(cause))
        },
    }
}

impl From<ForgejoLikeKind> for ForgeKind {
    fn from(kind: ForgejoLikeKind) -> Self {
        match kind {
            ForgejoLikeKind::Forgejo => Self::Forgejo,
            ForgejoLikeKind::Gitea => Self::Gitea,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CompareJob,
        CompareSource,
        ForgejoLikeKind,
    };
    use crate::{
        fetch::CompareStatus,
        source::id::SourceId,
    };

    fn source_id(raw: &str) -> SourceId {
        SourceId::from_url(raw).unwrap()
    }

    #[test]
    fn source_from_id_maps_first_class_forges() {
        assert!(matches!(
            CompareSource::from_source_id(&source_id("github:o/r")),
            Some(CompareSource::Github { .. })
        ));
        assert!(matches!(
            CompareSource::from_source_id(&source_id("gitlab:o/r")),
            Some(CompareSource::Gitlab { .. })
        ));
    }

    #[test]
    fn identical_compare_job_is_verified_without_network() {
        let session = super::CompareSession::new();
        let attempt = session.compare(&CompareJob {
            source: CompareSource::ForgejoLike {
                kind:  ForgejoLikeKind::Forgejo,
                host:  "git.example.com".to_owned(),
                owner: "o".to_owned(),
                repo:  "r".to_owned(),
            },
            base:   "same".to_owned(),
            head:   "same".to_owned(),
        });

        assert_eq!(attempt.status, Some(CompareStatus::Identical));
    }
}
