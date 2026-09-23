// SPDX-License-Identifier: EUPL-1.2

use std::{
    fmt::{
        Display,
        Formatter,
        Result as FmtResult,
    },
    path::{
        Path,
        PathBuf,
    },
    str::FromStr,
    time::Duration,
};

use misstep::Result;
use serde::{
    Deserialize,
    Serialize,
    de::DeserializeOwned,
};

use super::{
    FetchError,
    FetchResult,
    FetchedPin,
    archive::{
        TarFormat,
        unpack_tar_stream,
    },
    auth::with_credential_fallback,
    http::HttpClient,
    time::epoch_from_iso,
    topology::{
        CommitLog,
        CompareStatus,
    },
};
use crate::{
    error::user_bail,
    lock::LockedNode,
    nar,
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
        owner: &str,
        repo: &str,
        old: &str,
        new: &str,
        limit: usize,
    ) -> FetchResult<CommitLog> {
        let url = format!("https://api.github.com/repos/{owner}/{repo}/compare/{old}...{new}");
        let parsed = self.http.github_json::<GithubCompareResponse>(&url, None)?;
        Ok(parsed.commit_log(limit))
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
        with_credential_fallback("github.com", true, |credential| {
            let request = HttpClient::with_credential(self.http.get(&url), credential);
            let mut resp = request
                .call()
                .map_err(|err| FetchError::from_ureq(err, &url))?;
            if resp.status() != 200 {
                return Err(FetchError::from_response(&mut resp, &url));
            }
            unpack_tar_stream(resp.body_mut().as_reader(), TarFormat::Gz, into)
                .map_err(|err| FetchError::Transport(format!("download {url}: {err}")))
        })
        .map_err(|err| misstep::report!("download {url}: {err}"))
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
    total_commits: Option<u64>,
    #[serde(default)]
    ahead_by:      u64,
    #[serde(default)]
    behind_by:     u64,
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
            total,
            ahead: self.ahead_by,
            behind: self.behind_by,
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
) -> Result<FetchedPin> {
    let github = GithubClient::global();
    let resolved = github.resolve_for_pin(owner, repo, reff, pinned)?;
    let dir = tempfile::tempdir()?;
    let root = github.download_tarball(owner, repo, &resolved.rev, dir.path())?;
    let nar_hash = nar::hash_path(&root)?;
    let rev = resolved.rev;
    let node = LockedNode::new_github(owner, repo, rev.clone(), nar_hash, resolved.last_modified);
    Ok(FetchedPin::rev(node, rev).with_tree(dir, root))
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
        // github compares old rev against the ref
        "AHEAD" => CompareStatus::Behind,
        "BEHIND" => CompareStatus::Ahead,
        "DIVERGED" => CompareStatus::Diverged,
        "IDENTICAL" => CompareStatus::Identical,
        _ => return None,
    })
}

pub(super) fn resolve_ref_compare(
    owner: &str,
    repo: &str,
    reff: Option<&str>,
    base: &str,
) -> FetchResult<(String, Option<CompareStatus>)> {
    let compare = GithubClient::global().ref_compare(owner, repo, reff, base)?;
    Ok((compare.rev, compare.status))
}

pub(super) fn compare_ref_status(
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

pub(super) fn compare_status(
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
) -> FetchResult<Option<CompareStatus>> {
    GithubClient::global().compare_status(owner, repo, base, head)
}

pub(super) fn commits_between(
    owner: &str,
    repo: &str,
    old: &str,
    new: &str,
    limit: usize,
) -> FetchResult<CommitLog> {
    GithubClient::global().commits_between(owner, repo, old, new, limit)
}

const GITHUB_API: &str = "https://api.github.com/";
const GITHUB_PAGE_LIMIT: usize = 10;

/// follows `rel="next"` links within api.github.com, failing rather than
/// truncating once the page cap is hit
fn paged<T>(first: &str) -> FetchResult<Vec<T>>
where
    T: DeserializeOwned,
{
    let mut items = Vec::new();
    let mut url = first.to_owned();
    for _ in 0..GITHUB_PAGE_LIMIT {
        let (page, next) = GithubClient::global()
            .http
            .github_json_page::<Vec<T>>(&url, None)?;
        items.extend(page);
        match next {
            Some(link) if link.starts_with(GITHUB_API) => url = link,
            _ => return Ok(items),
        }
    }
    Err(FetchError::Github(format!(
        "{first} lists more than {GITHUB_PAGE_LIMIT} pages"
    )))
}

pub fn ssh_signing_keys(user: &GithubUser) -> FetchResult<Vec<String>> {
    let url = format!("{GITHUB_API}users/{user}/ssh_signing_keys?per_page=100");
    let keys = paged::<GithubSshKey>(&url)?;
    Ok(keys.into_iter().map(|key| key.key).collect())
}

/// armored keys, and how many entries came without one
pub struct GpgKeys {
    pub armored: Vec<String>,
    pub dropped: usize,
}

pub fn gpg_keys(user: &GithubUser) -> FetchResult<GpgKeys> {
    let url = format!("{GITHUB_API}users/{user}/gpg_keys?per_page=100");
    let keys = paged::<GithubGpgKey>(&url)?;
    let total = keys.len();
    let armored = keys
        .into_iter()
        .filter_map(|key| key.raw_key)
        .collect::<Vec<_>>();
    Ok(GpgKeys {
        dropped: total.saturating_sub(armored.len()),
        armored,
    })
}

/// a GitHub login, checked before it lands in an API path
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubUser(String);

impl FromStr for GithubUser {
    type Err = misstep::Report;

    fn from_str(raw: &str) -> Result<Self> {
        let valid = !raw.is_empty()
            && raw
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        if !valid {
            user_bail!("'{raw}' is not a GitHub user name");
        }
        Ok(Self(raw.to_owned()))
    }
}

impl Display for GithubUser {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(&self.0)
    }
}

#[derive(Deserialize)]
struct GithubSshKey {
    key: String,
}

#[derive(Deserialize)]
struct GithubGpgKey {
    raw_key: Option<String>,
}

pub fn pull_diff(owner: &str, repo: &str, number: u64) -> FetchResult<String> {
    diff_with_fallback(
        &format!("https://api.github.com/repos/{owner}/{repo}/pulls/{number}"),
        &format!("https://github.com/{owner}/{repo}/pull/{number}.diff"),
    )
}

pub fn commit_diff(owner: &str, repo: &str, rev: &str) -> FetchResult<String> {
    diff_with_fallback(
        &format!("https://api.github.com/repos/{owner}/{repo}/commits/{rev}"),
        &format!("https://github.com/{owner}/{repo}/commit/{rev}.diff"),
    )
}

/// the api refuses diffs of more than 300 files, which the web route serves
fn diff_with_fallback(api: &str, web: &str) -> FetchResult<String> {
    let http = GithubClient::global().http;
    http.github_diff(api)
        .or_else(|err| http.raw_text(web, Some("github.com")).map_err(|_| err))
}

#[cfg(test)]
#[path = "github_tests.rs"]
mod tests;
