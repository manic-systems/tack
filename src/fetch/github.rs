// SPDX-License-Identifier: EUPL-1.2

use std::{
    path::{
        Path,
        PathBuf,
    },
    time::Duration,
};

use eyre::{
    Result,
    WrapErr as _,
};
use serde::{
    Deserialize,
    Serialize,
};

use super::{
    archive::{
        TarFormat,
        unpack_tar_stream,
    },
    http::{
        FetchError,
        FetchResult,
        HttpClient,
    },
    time::epoch_from_iso,
    topology::CompareStatus,
};
use crate::{
    lock::LockedNode,
    nar,
    source::Source,
};

#[derive(Clone, Copy)]
struct GithubClient {
    http: HttpClient,
}

impl GithubClient {
    fn global() -> Self {
        Self {
            http: HttpClient::global(),
        }
    }

    fn commit(self, owner: &str, repo: &str, reff: &str) -> FetchResult<(String, i64)> {
        let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{reff}");
        let parsed = self.http.github_json::<GithubCommitResponse>(&url, None)?;
        parsed.rev_and_epoch(&format!("{owner}/{repo}@{reff}"))
    }

    fn compare_status(
        self,
        owner: &str,
        repo: &str,
        base: &str,
        head: &str,
    ) -> FetchResult<Option<CompareStatus>> {
        let url = Self::compare_url(owner, repo, base, head);
        let parsed = self
            .http
            .github_json::<GithubCompareResponse>(&url, Some(Duration::from_secs(5)))?;
        Ok(parsed.status())
    }

    fn ref_compare(
        self,
        owner: &str,
        repo: &str,
        reff: Option<&str>,
        old_rev: &str,
    ) -> FetchResult<GithubRefCompare> {
        let query = if reff.is_some() {
            GITHUB_REF_COMPARE_QUERY
        } else {
            GITHUB_DEFAULT_COMPARE_QUERY
        };
        let variables = GithubCompareVariables {
            owner,
            repo,
            old: old_rev,
            ref_name: reff,
        };
        let data = self.http.github_graphql(query, &variables)?;
        GithubRefCompareData::resolve(&data)
    }

    fn compare_url(owner: &str, repo: &str, base: &str, head: &str) -> String {
        format!("https://api.github.com/repos/{owner}/{repo}/compare/{base}...{head}?per_page=1")
    }

    fn commits_between(
        self,
        source: &Source,
        old: &str,
        new: &str,
        limit: usize,
    ) -> FetchResult<Option<CommitLog>> {
        let &Source::Github {
            ref owner,
            ref repo,
            ..
        } = source
        else {
            return Ok(None);
        };
        let url = format!("https://api.github.com/repos/{owner}/{repo}/compare/{old}...{new}");
        let parsed = self.http.github_json::<GithubCompareResponse>(&url, None)?;
        Ok(Some(parsed.commit_log(limit)))
    }

    fn resolve_for_pin(
        self,
        owner: &str,
        repo: &str,
        reff: Option<&str>,
        pinned: Option<String>,
    ) -> Result<ResolvedGithubRef> {
        if let Some(rev) = pinned {
            let (_, last_modified) = self.commit(owner, repo, &rev)?;
            return Ok(ResolvedGithubRef { rev, last_modified });
        }

        let ref_str = reff.unwrap_or("HEAD");
        let (rev, last_modified) = self.commit(owner, repo, ref_str)?;
        Ok(ResolvedGithubRef { rev, last_modified })
    }

    fn download_tarball(self, owner: &str, repo: &str, rev: &str, into: &Path) -> Result<PathBuf> {
        let url = format!("https://codeload.github.com/{owner}/{repo}/tar.gz/{rev}");
        let mut resp = self
            .http
            .get(&url)
            .call()
            .wrap_err_with(|| format!("download {url}"))?;
        unpack_tar_stream(resp.body_mut().as_reader(), TarFormat::Gz, into)
    }
}

#[derive(Deserialize)]
struct GithubCommitResponse {
    sha:    Option<String>,
    #[serde(default)]
    commit: GithubCommitBody,
}

impl GithubCommitResponse {
    fn rev_and_epoch(&self, what: &str) -> FetchResult<(String, i64)> {
        let rev = self
            .sha
            .as_deref()
            .ok_or_else(|| FetchError::Github(format!("no sha in response for {what}")))?
            .to_owned();
        let date = self
            .commit
            .committer
            .date
            .as_deref()
            .ok_or_else(|| FetchError::Github(format!("no commit date for {what}")))?;
        let epoch = epoch_from_iso(date)
            .map_err(|err| FetchError::Github(format!("bad commit date for {what}: {err}")))?;
        Ok((rev, epoch))
    }
}

#[derive(Default, Deserialize)]
struct GithubCommitBody {
    #[serde(default)]
    committer: GithubCommitter,
    message:   Option<String>,
}

#[derive(Default, Deserialize)]
struct GithubCommitter {
    date: Option<String>,
}

#[derive(Deserialize)]
struct GithubCompareResponse {
    status:        Option<String>,
    #[serde(default)]
    commits:       Vec<GithubCompareCommit>,
    #[serde(rename = "total_commits")]
    total_commits: Option<u64>,
    base_commit:   Option<GithubCompareCommit>,
}

impl GithubCompareResponse {
    fn status(&self) -> Option<CompareStatus> {
        self.status.as_deref()?.parse().ok()
    }

    fn commit_log(&self, limit: usize) -> CommitLog {
        let total = self
            .total_commits
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(self.commits.len());
        let fresh = self
            .commits
            .iter()
            .rev()
            .take(limit)
            .filter_map(GithubCompareCommit::pair)
            .collect::<Vec<_>>();
        let base = self
            .base_commit
            .as_ref()
            .and_then(GithubCompareCommit::pair);
        CommitLog {
            fresh,
            base,
            more: total > limit,
        }
    }
}

#[derive(Deserialize)]
struct GithubCompareCommit {
    sha:    Option<String>,
    #[serde(default)]
    commit: GithubCommitBody,
}

impl GithubCompareCommit {
    fn pair(&self) -> Option<(String, String)> {
        let sha = self.sha.as_ref()?.clone();
        let msg = self.commit.message.as_deref()?;
        let subject = msg.lines().next().unwrap_or("").trim_end().to_owned();
        Some((sha, subject))
    }
}

pub(super) fn current_rev(
    owner: &str,
    repo: &str,
    reff: Option<&str>,
    pinned: Option<&str>,
) -> Result<String> {
    let github = GithubClient::global();
    pinned.map_or_else(
        || Ok(github.commit(owner, repo, reff.unwrap_or("HEAD"))?.0),
        |rev| Ok(rev.to_owned()),
    )
}

pub(super) fn fetch_locked_tree_into(
    owner: &str,
    repo: &str,
    rev: &str,
    dir: &Path,
) -> Result<PathBuf> {
    GithubClient::global().download_tarball(owner, repo, rev, dir)
}

pub(super) fn fetch_tree_into(
    owner: &str,
    repo: &str,
    reff: Option<&str>,
    pinned: Option<&str>,
    dir: &Path,
) -> Result<PathBuf> {
    let github = GithubClient::global();
    let tree_rev = if let Some(pinned_rev) = pinned {
        pinned_rev.to_owned()
    } else {
        let ref_str = reff.unwrap_or("HEAD");
        github.commit(owner, repo, ref_str)?.0
    };
    github.download_tarball(owner, repo, &tree_rev, dir)
}

pub(super) fn fetch_pin(
    owner: &str,
    repo: &str,
    reff: Option<&str>,
    pinned: Option<String>,
) -> Result<(LockedNode, String)> {
    let github = GithubClient::global();
    let resolved = github.resolve_for_pin(owner, repo, reff, pinned)?;
    let dir = tempfile::tempdir()?;
    let root = github.download_tarball(owner, repo, &resolved.rev, dir.path())?;
    let nar_hash = nar::hash_path(&root)?;
    let rev = resolved.rev;
    let node = LockedNode::new_github(owner, repo, rev.clone(), nar_hash, resolved.last_modified);
    Ok((node, rev))
}

struct ResolvedGithubRef {
    rev:           String,
    last_modified: i64,
}

#[derive(Serialize)]
struct GithubCompareVariables<'a> {
    owner:    &'a str,
    repo:     &'a str,
    old:      &'a str,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    ref_name: Option<&'a str>,
}

struct GithubRefCompare {
    rev:    String,
    status: Option<CompareStatus>,
}

#[derive(Deserialize)]
struct GithubRefCompareData {
    repository: Option<GithubRefCompareRepository>,
}

impl GithubRefCompareData {
    fn resolve(&self) -> FetchResult<GithubRefCompare> {
        let ref_node = self
            .repository
            .as_ref()
            .and_then(|repo| repo.target_ref.as_ref())
            .ok_or_else(|| FetchError::Github("graphql response missing ref".to_owned()))?;
        let target = ref_node
            .target
            .as_ref()
            .ok_or_else(|| FetchError::Github("graphql response missing ref target".to_owned()))?;
        let rev = target.commit_oid()?;
        let status = ref_node
            .compare
            .as_ref()
            .and_then(|compare| compare.status.as_deref())
            .and_then(graphql_ref_compare_status);
        Ok(GithubRefCompare { rev, status })
    }
}

#[derive(Deserialize)]
struct GithubRefCompareRepository {
    #[serde(rename = "targetRef")]
    target_ref: Option<GithubRefNode>,
}

#[derive(Deserialize)]
struct GithubRefNode {
    target:  Option<GithubRefTarget>,
    compare: Option<GithubRefComparison>,
}

#[derive(Deserialize)]
struct GithubRefComparison {
    status: Option<String>,
}

#[derive(Deserialize)]
struct GithubRefTarget {
    oid:            Option<String>,
    #[serde(rename = "committedDate")]
    committed_date: Option<String>,
    target:         Option<Box<Self>>,
}

impl GithubRefTarget {
    fn commit_oid(&self) -> FetchResult<String> {
        let commit = self
            .target
            .as_deref()
            .filter(|inner| inner.committed_date.is_some())
            .unwrap_or(self);
        commit
            .oid
            .as_deref()
            .ok_or_else(|| FetchError::Github("graphql response missing commit oid".to_owned()))
            .map(str::to_owned)
    }
}

const GITHUB_REF_COMPARE_QUERY: &str = "
query($owner: String!, $repo: String!, $ref: String!, $old: String!) {
  repository(owner: $owner, name: $repo) {
    targetRef: ref(qualifiedName: $ref) {
      target {
        oid
        ... on Commit { committedDate }
        ... on Tag {
          target {
            oid
            ... on Commit { committedDate }
          }
        }
      }
      compare(headRef: $old) {
        status
        aheadBy
        behindBy
      }
    }
  }
}
";

const GITHUB_DEFAULT_COMPARE_QUERY: &str = "
query($owner: String!, $repo: String!, $old: String!) {
  repository(owner: $owner, name: $repo) {
    targetRef: defaultBranchRef {
      target {
        oid
        ... on Commit { committedDate }
        ... on Tag {
          target {
            oid
            ... on Commit { committedDate }
          }
        }
      }
      compare(headRef: $old) {
        status
        aheadBy
        behindBy
      }
    }
  }
}
";

fn graphql_ref_compare_status(status: &str) -> Option<CompareStatus> {
    Some(match status {
        // ref.compare uses targetRef as base and old locked rev as head, but tack
        // shows current ref relative to old rev, so ahead/behind invert.
        "AHEAD" => CompareStatus::Behind,
        "BEHIND" => CompareStatus::Ahead,
        "DIVERGED" => CompareStatus::Diverged,
        "IDENTICAL" => CompareStatus::Identical,
        _ => return None,
    })
}

pub fn resolve_ref_compare(
    owner: &str,
    repo: &str,
    reff: Option<&str>,
    base: &str,
) -> FetchResult<(String, Option<CompareStatus>)> {
    let compare = GithubClient::global().ref_compare(owner, repo, reff, base)?;
    Ok((compare.rev, compare.status))
}

/// compare the resolved GitHub ref against base, returning none when the ref no
/// longer resolves to the expected head or the GraphQL status is unrecognized.
pub fn compare_ref_status(
    owner: &str,
    repo: &str,
    reff: Option<&str>,
    base: &str,
    head: &str,
) -> FetchResult<Option<CompareStatus>> {
    let compare = GithubClient::global().ref_compare(owner, repo, reff, base)?;
    if compare.rev == head {
        Ok(compare.status)
    } else {
        Ok(None)
    }
}

/// compare head against base; none when the status is unrecognized
pub fn compare_status(
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
) -> FetchResult<Option<CompareStatus>> {
    GithubClient::global().compare_status(owner, repo, base, head)
}

#[derive(Clone, Debug)]
pub struct CommitLog {
    /// freshest commits in the range, newest first
    pub fresh: Vec<(String, String)>,
    /// the currently-pinned (base) commit
    pub base:  Option<(String, String)>,
    /// more than the limit existed
    pub more:  bool,
}

/// fresh commits between old and new, capped at limit; none for non-github
/// targets with no clone-free path yet
pub fn commits_between(
    source: &Source,
    old: &str,
    new: &str,
    limit: usize,
) -> FetchResult<Option<CommitLog>> {
    GithubClient::global().commits_between(source, old, new, limit)
}

#[cfg(test)]
mod tests {
    use super::{
        CompareStatus,
        GithubClient,
        GithubRefCompareData,
        graphql_ref_compare_status,
    };
    use crate::nar;

    #[test]
    fn graphql_ref_compare_status_is_inverted_for_tack_display() {
        assert_eq!(
            graphql_ref_compare_status("BEHIND"),
            Some(CompareStatus::Ahead)
        );
        assert_eq!(
            graphql_ref_compare_status("AHEAD"),
            Some(CompareStatus::Behind)
        );
        assert_eq!(
            graphql_ref_compare_status("DIVERGED"),
            Some(CompareStatus::Diverged)
        );
        assert_eq!(
            graphql_ref_compare_status("IDENTICAL"),
            Some(CompareStatus::Identical)
        );
        assert_eq!(graphql_ref_compare_status("UNKNOWN"), None);
    }

    #[test]
    fn parses_graphql_ref_compare_response() {
        let parsed = serde_json::from_str::<GithubRefCompareData>(
            r#"{
                "repository": {
                    "targetRef": {
                        "target": {
                            "oid": "new",
                            "committedDate": "2026-05-30T18:08:13Z"
                        },
                        "compare": {
                            "status": "BEHIND",
                            "aheadBy": 0,
                            "behindBy": 1264
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        let resolved = parsed.resolve().unwrap();

        assert_eq!(resolved.rev, "new");
        assert_eq!(resolved.status, Some(CompareStatus::Ahead));
    }

    #[test]
    fn parses_graphql_annotated_tag_target() {
        let parsed = serde_json::from_str::<GithubRefCompareData>(
            r#"{
                "repository": {
                    "targetRef": {
                        "target": {
                            "oid": "tag-object",
                            "target": {
                                "oid": "commit",
                                "committedDate": "2026-05-30T18:08:13Z"
                            }
                        },
                        "compare": {
                            "status": "IDENTICAL"
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        let resolved = parsed.resolve().unwrap();

        assert_eq!(resolved.rev, "commit");
        assert_eq!(resolved.status, Some(CompareStatus::Identical));
    }

    fn commit(
        repo: &gix::Repository,
        parent_ids: &[gix::ObjectId],
        message: &str,
        time: i64,
    ) -> gix::ObjectId {
        let signature_text = format!("tack <tack@example.invalid> {time} +0000");
        let signature = gix::actor::SignatureRef::from_bytes(signature_text.as_bytes()).unwrap();
        repo.new_commit_as(
            signature,
            signature,
            message,
            gix::ObjectId::empty_tree(repo.object_hash()),
            parent_ids.iter().copied(),
        )
        .unwrap()
        .id()
        .detach()
    }

    fn local_compare(
        repo: &gix::Repository,
        base: gix::ObjectId,
        head: gix::ObjectId,
    ) -> CompareStatus {
        if base == head {
            return CompareStatus::Identical;
        }
        let merge_base = repo.merge_base(base, head).unwrap().detach();
        let base_is_ancestor = merge_base == base;
        let head_is_ancestor = merge_base == head;
        CompareStatus::from_ancestry(base_is_ancestor, head_is_ancestor)
    }

    #[test]
    fn compare_status_from_local_merge_base_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = gix::init(tmp.path()).unwrap();
        let root = commit(&repo, &[], "root", 100);
        let base = commit(&repo, &[root], "base", 300);
        let ahead_with_older_timestamp = commit(&repo, &[base], "ahead", 200);
        let amended_with_newer_timestamp = commit(&repo, &[root], "amended", 400);

        assert_eq!(
            local_compare(&repo, base, ahead_with_older_timestamp),
            CompareStatus::Ahead
        );
        assert_eq!(local_compare(&repo, base, root), CompareStatus::Behind);
        assert_eq!(local_compare(&repo, base, base), CompareStatus::Identical);
        assert_eq!(
            local_compare(&repo, base, amended_with_newer_timestamp),
            CompareStatus::Diverged
        );
    }

    // our tarball nar hash must equal nix's narHash for this rev
    #[test]
    #[cfg_attr(not(feature = "network-tests"), ignore = "hits codeload.github.com")]
    fn github_narhash_matches_nix() {
        let dir = tempfile::tempdir().unwrap();
        let root = GithubClient::global()
            .download_tarball(
                "bertof",
                "nix-rice",
                "98b16b0f649bb41db9a1c3b32191bccb9a1ec271",
                dir.path(),
            )
            .unwrap();
        assert_eq!(
            nar::hash_path(&root).unwrap(),
            "sha256-nt/xmuXaJB/vWlRJ4wpdlYQCIgCzFR6QJwlRyhfNn5o="
        );
    }
}
