// SPDX-License-Identifier: EUPL-1.2

use std::{
    env,
    error::Error,
    fs,
    io::Read,
    ops::Range,
    path::{
        Path,
        PathBuf,
    },
    result::Result as StdResult,
    str::FromStr,
    sync::OnceLock,
    time::Duration,
};

use eyre::{
    ContextCompat as _,
    Result,
    WrapErr as _,
    bail,
    eyre,
};
use flate2::read::GzDecoder;
use git2::{
    Cred,
    CredentialType,
    Direction,
    FetchOptions,
    RemoteCallbacks,
    Repository,
    build::CheckoutBuilder,
};
use serde_json::{
    Value,
    json,
};
use ureq::{
    Agent,
    Body,
    ResponseExt as _,
    http,
    tls::{
        TlsConfig,
        TlsProvider,
    },
};
use xz2::read::XzDecoder;

use crate::{
    nar,
    pins::Unpack,
    source::Source,
};

fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let config = TlsConfig::builder()
            .provider(TlsProvider::NativeTls)
            .build();
        Agent::config_builder().tls_config(config).build().into()
    })
}

fn github_token() -> Option<String> {
    env::var("GITHUB_TOKEN")
        .or_else(|_| env::var("GH_TOKEN"))
        .ok()
}

type FetchResult<T> = StdResult<T, FetchError>;

/// Why a github query or raw-file probe failed. Callers that fall back on
/// degraded operations match on this to tolerate expected failures while
/// surfacing the ones a user must fix.
#[derive(thiserror::Error, Debug)]
pub enum FetchError {
    /// A ref, commit, repo, or file that genuinely is not there.
    #[error("{what} not found")]
    NotFound { what: String },
    /// GitHub rejected or lacked credentials.
    #[error("github auth: {what}")]
    Auth { what: String },
    /// Could not reach the host, or the host returned a transient server error.
    #[error("network: {0}")]
    Transport(String),
    /// A fetched raw-file body could not be decoded.
    #[error("decoding {what}")]
    Decode {
        what:   String,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    /// Upstream returned a response shape this version of tack does not know.
    #[error("unexpected upstream response: {0}")]
    Upstream(String),
}

fn http_error(status: u16, what: &str) -> FetchError {
    match status {
        401 | 403 => {
            FetchError::Auth {
                what: what.to_owned(),
            }
        },
        404 => {
            FetchError::NotFound {
                what: what.to_owned(),
            }
        },
        other => FetchError::Transport(format!("{what}: HTTP {other}")),
    }
}

/// classify a ureq error. ureq surfaces non-2xx responses as
/// [`ureq::Error::StatusCode`] by default, so auth/not-found classification has
/// to happen here too, not only on a returned response.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "ureq::Error is #[non_exhaustive]"
)]
fn from_ureq(err: ureq::Error, what: &str) -> FetchError {
    match err {
        ureq::Error::StatusCode(code) => http_error(code, what),
        other => FetchError::Transport(format!("{what}: {other}")),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BranchComparison {
    pub status:   Option<CompareStatus>,
    pub expected: bool,
}

impl BranchComparison {
    pub const fn none() -> Self {
        Self {
            status:   None,
            expected: false,
        }
    }

    pub const fn verified(status: CompareStatus) -> Self {
        Self {
            status:   Some(status),
            expected: true,
        }
    }

    pub const fn unavailable() -> Self {
        Self {
            status:   None,
            expected: true,
        }
    }
}

pub struct CurrentRev {
    pub rev:        String,
    pub comparison: BranchComparison,
}

/// upstream rev plus, when possible, branch topology against `old_rev`.
pub fn current_rev_compared(source: &Source, old_rev: Option<&str>) -> Result<CurrentRev> {
    match *source {
        Source::Github {
            ref owner,
            ref repo,
            ref reff,
            rev: ref pinned,
        } => {
            if let Some(pinned_rev) = pinned.as_deref() {
                let comparison = if old_rev == Some(pinned_rev) {
                    BranchComparison::verified(CompareStatus::Identical)
                } else {
                    BranchComparison::none()
                };
                return Ok(CurrentRev {
                    rev: pinned_rev.to_owned(),
                    comparison,
                });
            }
            let ref_str = reff.as_deref().unwrap_or("HEAD");
            if let Some(previous_rev) = old_rev
                && let Ok(resolved) = gh_ref_compare(owner, repo, reff.as_deref(), previous_rev)
            {
                let filled = backfill_comparison(owner, repo, previous_rev, resolved);
                return Ok(CurrentRev {
                    rev:        filled.rev,
                    comparison: filled.comparison,
                });
            }
            let (rev, _) = gh_commit(owner, repo, ref_str)?;
            let comparison = old_rev.map_or_else(BranchComparison::none, |previous_rev| {
                if previous_rev == rev.as_str() {
                    BranchComparison::verified(CompareStatus::Identical)
                } else {
                    compare_status(owner, repo, previous_rev, &rev)
                        .ok()
                        .flatten()
                        .map_or_else(BranchComparison::unavailable, BranchComparison::verified)
                }
            });
            Ok(CurrentRev { rev, comparison })
        },
        Source::Git {
            ref url,
            ref reff,
            rev: ref pinned,
        } => {
            // a pinned rev never moves; report it without touching the network
            if let Some(pinned_rev) = pinned.as_deref() {
                let comparison = if old_rev == Some(pinned_rev) {
                    BranchComparison::verified(CompareStatus::Identical)
                } else {
                    BranchComparison::none()
                };
                return Ok(CurrentRev {
                    rev: pinned_rev.to_owned(),
                    comparison,
                });
            }
            let cb = callbacks();
            let mut remote = git2::Remote::create_detached(url.as_str())?;
            let conn = remote.connect_auth(Direction::Fetch, Some(cb), None)?;
            let want = full_ref(reff.as_deref(), || branch_str(conn.default_branch()));
            for head in conn.list()? {
                if head.name() == want {
                    let head_rev = head.oid().to_string();
                    let comparison = if old_rev == Some(head_rev.as_str()) {
                        BranchComparison::verified(CompareStatus::Identical)
                    } else {
                        BranchComparison::none()
                    };
                    return Ok(CurrentRev {
                        rev: head_rev,
                        comparison,
                    });
                }
            }
            bail!("ref {want} not found on {url}")
        },
        Source::Tarball { ref url } => {
            let resp = agent()
                .head(url.as_str())
                .header("User-Agent", "tack")
                .call()
                .or_else(|_| {
                    agent()
                        .get(url.as_str())
                        .header("User-Agent", "tack")
                        .call()
                        .map_err(Box::new)
                })
                .wrap_err_with(|| format!("probe {url}"))?;
            let rev = immutable_url_of(&resp, url);
            let comparison = if old_rev == Some(rev.as_str()) {
                BranchComparison::verified(CompareStatus::Identical)
            } else {
                BranchComparison::none()
            };
            Ok(CurrentRev { rev, comparison })
        },
    }
}

/// Fetch a `fixed` pin: download URL bytes, sha256 them as raw bytes (not NAR),
/// return the locked node plus the sha256 (used for the drift-display "rev").
/// Auto-detects `unpack` from URL extension when not supplied.
pub fn fetch_fixed_pin(url: &str, unpack: Option<Unpack>) -> Result<(Value, String)> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        bail!("fixed pins require a plain http(s) URL, got: {url}");
    }
    let mut resp = agent()
        .get(url)
        .header("User-Agent", "tack")
        .call()
        .wrap_err_with(|| format!("GET {url}"))?;
    let immutable_url = immutable_url_of(&resp, url);
    let mut bytes = Vec::new();
    resp.body_mut()
        .as_reader()
        .read_to_end(&mut bytes)
        .wrap_err_with(|| format!("read body of {url}"))?;
    let sha256 = nar::hash_bytes(&bytes);
    // detect from the user-supplied URL first; the immutable URL may have lost
    // the extension via a redirect (e.g. github archives -> codeload)
    let kind = unpack.unwrap_or_else(|| {
        if Unpack::detect(url) == Unpack::Tarball
            || Unpack::detect(&immutable_url) == Unpack::Tarball
        {
            Unpack::Tarball
        } else {
            Unpack::File
        }
    });
    let node = json!({
        "type": "fixed",
        "url": immutable_url,
        "sha256": sha256,
        "unpack": kind.as_str(),
    });
    Ok((node, sha256))
}

/// download a locked tree into `dir` for inspection; no narhash, no metadata.
/// fixed pins are flat content, not trees — caller skips those.
pub fn fetch_locked_tree_into(node: &Value, dir: &Path) -> Result<PathBuf> {
    let ty = node
        .get("type")
        .and_then(Value::as_str)
        .context("lock node missing type")?;
    match ty {
        "github" => {
            let owner = node
                .get("owner")
                .and_then(Value::as_str)
                .context("github node missing owner")?;
            let repo = node
                .get("repo")
                .and_then(Value::as_str)
                .context("github node missing repo")?;
            let rev = node
                .get("rev")
                .and_then(Value::as_str)
                .context("github node missing rev")?;
            download_github_tarball(owner, repo, rev, dir)
        },
        "git" => {
            let url = node
                .get("url")
                .and_then(Value::as_str)
                .context("git node missing url")?;
            let reff = node.get("ref").and_then(Value::as_str);
            let rev = node.get("rev").and_then(Value::as_str);
            let submodules = node
                .get("submodules")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            git_checkout(url, reff, rev, submodules, dir)?;
            let _ = fs::remove_dir_all(dir.join(".git"));
            Ok(dir.to_owned())
        },
        "tarball" => {
            let url = node
                .get("url")
                .and_then(Value::as_str)
                .context("tarball node missing url")?;
            let mut resp = agent()
                .get(url)
                .header("User-Agent", "tack")
                .call()
                .wrap_err_with(|| format!("GET {url}"))?;
            let format = detect_tar_format(url).wrap_err_with(|| format!("tarball {url}"))?;
            unpack_tar_stream(resp.body_mut().as_reader(), format, dir)
        },
        other => bail!("cannot inspect tree for lock type '{other}'"),
    }
}

/// fetch a tree by parsed URL into `dir`; no narhash, no metadata.
/// used when traversing tack transitives that have no committed lock.
pub fn fetch_tree_into(source: &Source, submodules: bool, dir: &Path) -> Result<PathBuf> {
    match *source {
        Source::Github {
            ref owner,
            ref repo,
            ref reff,
            rev: ref pinned,
        } => {
            let tree_rev = if let Some(pinned_rev) = pinned.as_deref() {
                pinned_rev.to_owned()
            } else {
                let ref_str = reff.as_deref().unwrap_or("HEAD");
                gh_commit(owner, repo, ref_str)?.0
            };
            download_github_tarball(owner, repo, &tree_rev, dir)
        },
        Source::Git {
            ref url,
            ref reff,
            ref rev,
        } => {
            git_checkout(url, reff.as_deref(), rev.as_deref(), submodules, dir)?;
            let _ = fs::remove_dir_all(dir.join(".git"));
            Ok(dir.to_owned())
        },
        Source::Tarball { ref url } => {
            let mut resp = agent()
                .get(url.as_str())
                .header("User-Agent", "tack")
                .call()
                .wrap_err_with(|| format!("GET {url}"))?;
            let format = detect_tar_format(url).wrap_err_with(|| format!("tarball {url}"))?;
            unpack_tar_stream(resp.body_mut().as_reader(), format, dir)
        },
    }
}

/// fetch the tree, return (locked node, rev)
pub fn fetch_pin(source: &Source, submodules: bool) -> Result<(Value, String)> {
    fetch_pin_compared(source, submodules, None).map(|fetched| (fetched.node, fetched.rev))
}

pub struct FetchedPin {
    pub node:       Value,
    pub rev:        String,
    pub comparison: BranchComparison,
}

/// fetch the tree, returning branch topology against `old_rev` when available.
pub fn fetch_pin_compared(
    source: &Source,
    submodules: bool,
    old_rev: Option<&str>,
) -> Result<FetchedPin> {
    match *source {
        Source::Github {
            ref owner,
            ref repo,
            ref reff,
            rev: ref pinned,
        } => fetch_github_pin_compared(owner, repo, reff.as_deref(), pinned.clone(), old_rev),
        Source::Git {
            ref url,
            ref reff,
            rev: ref rev_arg,
        } => {
            let dir = tempfile::tempdir()?;
            let (rev, last_modified, refname) = git_checkout(
                url,
                reff.as_deref(),
                rev_arg.as_deref(),
                submodules,
                dir.path(),
            )?;
            let _ = fs::remove_dir_all(dir.path().join(".git")).ok();
            let nar_hash = nar::hash_path(dir.path())?;
            let mut node = json!({
                "type": "git",
                "url": url,
                "ref": refname,
                "rev": rev,
                "narHash": nar_hash,
                "lastModified": last_modified,
            });
            if submodules {
                node["submodules"] = json!(true);
            }
            Ok(FetchedPin {
                node,
                rev,
                comparison: BranchComparison::none(),
            })
        },
        Source::Tarball { ref url } => {
            let mut resp = agent()
                .get(url.as_str())
                .header("User-Agent", "tack")
                .call()
                .wrap_err_with(|| format!("GET {url}"))?;
            let immutable_url = immutable_url_of(&resp, url);
            let last_modified = resp
                .headers()
                .get("Last-Modified")
                .and_then(|header| header.to_str().ok())
                .and_then(|header| epoch_from_http_date(header).ok())
                .unwrap_or(0);
            let format = detect_tar_format(&immutable_url)
                .or_else(|_| detect_tar_format(url))
                .wrap_err_with(|| format!("tarball {url}"))?;

            let dir = tempfile::tempdir()?;
            let root = unpack_tar_stream(resp.body_mut().as_reader(), format, dir.path())?;
            let nar_hash = nar::hash_path(&root)?;
            let node = json!({
                "type": "tarball",
                "url": immutable_url,
                "narHash": nar_hash,
                "lastModified": last_modified,
            });
            Ok(FetchedPin {
                node,
                rev: immutable_url,
                comparison: BranchComparison::none(),
            })
        },
    }
}

fn fetch_github_pin_compared(
    owner: &str,
    repo: &str,
    reff: Option<&str>,
    pinned: Option<String>,
    old_rev: Option<&str>,
) -> Result<FetchedPin> {
    let resolved = resolve_github_for_pin(owner, repo, reff, pinned, old_rev)?;
    let dir = tempfile::tempdir()?;
    let root = download_github_tarball(owner, repo, &resolved.rev, dir.path())?;
    let nar_hash = nar::hash_path(&root)?;
    let rev = resolved.rev;
    let node = json!({
        "type": "github",
        "owner": owner,
        "repo": repo,
        "rev": rev,
        "narHash": nar_hash,
        "lastModified": resolved.last_modified,
    });
    Ok(FetchedPin {
        node,
        rev,
        comparison: resolved.comparison,
    })
}

fn resolve_github_for_pin(
    owner: &str,
    repo: &str,
    reff: Option<&str>,
    pinned: Option<String>,
    old_rev: Option<&str>,
) -> Result<ResolvedGithubRef> {
    if let Some(rev) = pinned {
        let (_, last_modified) = gh_commit(owner, repo, &rev)?;
        return Ok(ResolvedGithubRef {
            rev,
            last_modified,
            comparison: BranchComparison::none(),
        });
    }

    let ref_str = reff.unwrap_or("HEAD");
    if let Some(previous_rev) = old_rev
        && let Ok(resolved) = gh_ref_compare(owner, repo, reff, previous_rev)
    {
        return Ok(backfill_comparison(owner, repo, previous_rev, resolved));
    }

    let (rev, last_modified) = gh_commit(owner, repo, ref_str)?;
    let comparison = old_rev.map_or_else(BranchComparison::none, |previous_rev| {
        if previous_rev == rev.as_str() {
            BranchComparison::verified(CompareStatus::Identical)
        } else {
            compare_status(owner, repo, previous_rev, &rev)
                .ok()
                .flatten()
                .map_or_else(BranchComparison::unavailable, BranchComparison::verified)
        }
    });
    Ok(ResolvedGithubRef {
        rev,
        last_modified,
        comparison,
    })
}

/// Locked URL for a tarball response
fn immutable_url_of(resp: &http::Response<Body>, fallback: &str) -> String {
    resp.headers()
        .get("Link")
        .and_then(|header| header.to_str().ok())
        .and_then(parse_link_immutable)
        .unwrap_or_else(|| {
            let uri = resp.get_uri().to_string();
            if uri.is_empty() {
                fallback.to_owned()
            } else {
                uri
            }
        })
}

/// Extract the immutable URL from an HTTP Link header per RFC 8288.
fn parse_link_immutable(header: &str) -> Option<String> {
    for raw_part in header.split(',') {
        let part = raw_part.trim();
        let (url_part, params) = part.split_once(';')?;
        let url = url_part
            .trim()
            .strip_prefix('<')
            .and_then(|inner| inner.strip_suffix('>'))?;
        for param in params.split(';') {
            let (key, raw_value) = param.trim().split_once('=')?;
            if key.trim().eq_ignore_ascii_case("rel") {
                let rel = raw_value.trim().trim_matches('"');
                if rel == "immutable" || rel == "immutable_link" {
                    return Some(url.to_owned());
                }
            }
        }
    }
    None
}

#[derive(Clone, Copy)]
enum TarFormat {
    Gz,
    Xz,
    Plain,
}

fn detect_tar_format(url: &str) -> Result<TarFormat> {
    let after_query = url.split('?').next().unwrap_or(url);
    let path = after_query.split('#').next().unwrap_or(after_query);
    if ends_with_ci(path, ".tar.xz") || ends_with_ci(path, ".txz") {
        Ok(TarFormat::Xz)
    } else if ends_with_ci(path, ".tar.gz") || ends_with_ci(path, ".tgz") {
        Ok(TarFormat::Gz)
    } else if ends_with_ci(path, ".tar") {
        Ok(TarFormat::Plain)
    } else {
        bail!(
            "cannot infer tarball format from url (need .tar, .tar.gz/.tgz, or .tar.xz/.txz): \
             {url}"
        )
    }
}

/// case-insensitive ASCII suffix check that is bytes-based to dodge utf-8
/// slicing
fn ends_with_ci(path: &str, ext: &str) -> bool {
    let pb = path.as_bytes();
    let eb = ext.as_bytes();
    pb.len() >= eb.len() && pb[pb.len() - eb.len()..].eq_ignore_ascii_case(eb)
}

/// Unpack a tarball stream into `into`, strip the single top-level directory
/// and return the stripped root.
fn unpack_tar_stream<R>(reader: R, format: TarFormat, into: &Path) -> Result<PathBuf>
where
    R: Read,
{
    let decompressed: Box<dyn Read> = match format {
        TarFormat::Gz => Box::new(GzDecoder::new(reader)),
        TarFormat::Xz => Box::new(XzDecoder::new(reader)),
        TarFormat::Plain => Box::new(reader),
    };
    let mut archive = tar::Archive::new(decompressed);
    archive.set_preserve_permissions(true);
    archive
        .unpack(into)
        .wrap_err_with(|| format!("unpack into {}", into.display()))?;
    let mut dirs = fs::read_dir(into)?
        .filter_map(|entry| entry.ok().map(|item| item.path()))
        .filter(|path| path.is_dir());
    let root = dirs.next().ok_or_else(|| eyre!("empty tarball"))?;
    if dirs.next().is_some() {
        bail!("unexpected multiple top-level dirs in tarball");
    }
    Ok(root)
}

fn gh_get(url: &str) -> FetchResult<Value> {
    gh_get_with_timeout(url, None)
}

fn gh_get_with_timeout(url: &str, timeout_limit: Option<Duration>) -> FetchResult<Value> {
    let mut req = agent()
        .get(url)
        .header("User-Agent", "tack")
        .header("Accept", "application/vnd.github+json");
    if let Some(token) = github_token() {
        req = req.header("Authorization", &format!("Bearer {token}"));
    }
    if let Some(timeout) = timeout_limit {
        req = req.config().timeout_global(Some(timeout)).build();
    }
    let mut resp = req.call().map_err(|err| from_ureq(err, url))?;
    let status = resp.status();
    if status != 200 {
        return Err(http_error(status.as_u16(), url));
    }
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|err| FetchError::Transport(format!("read github api {url}: {err}")))?;
    serde_json::from_str(&body)
        .map_err(|err| FetchError::Upstream(format!("github api {url}: invalid json: {err}")))
}

fn gh_graphql(query: &str, variables: &Value) -> FetchResult<Value> {
    let token = github_token().ok_or_else(|| {
        FetchError::Auth {
            what: "GITHUB_TOKEN or GH_TOKEN not set".to_owned(),
        }
    })?;
    let payload = json!({
        "query": query,
        "variables": variables,
    })
    .to_string();
    let mut resp = agent()
        .post("https://api.github.com/graphql")
        .header("User-Agent", "tack")
        .header("Accept", "application/vnd.github+json")
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {token}"))
        .config()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .send(payload)
        .map_err(|err| from_ureq(err, "github graphql"))?;
    let status = resp.status();
    if status != 200 {
        return Err(http_error(status.as_u16(), "github graphql"));
    }
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|err| FetchError::Transport(format!("read github graphql: {err}")))?;
    let parsed = serde_json::from_str::<Value>(&body)
        .map_err(|err| FetchError::Upstream(format!("github graphql invalid json: {err}")))?;
    if let Some(error) = parsed
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
    {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("github graphql error");
        if graphql_error_is_auth(error) {
            return Err(FetchError::Auth {
                what: message.to_owned(),
            });
        }
        return Err(FetchError::Upstream(format!("github graphql: {message}")));
    }
    Ok(parsed)
}

fn graphql_error_is_auth(error: &Value) -> bool {
    error
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "FORBIDDEN" | "UNAUTHORIZED"))
        || error
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| {
                let lower = message.to_ascii_lowercase();
                lower.contains("bad credentials")
                    || lower.contains("forbidden")
                    || lower.contains("unauthorized")
            })
}

/// direction of `head` relative to `base`, as reported by github's compare
/// endpoint
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareStatus {
    /// head has commits base lacks (head is newer)
    Ahead,
    /// head is missing commits base has (head is older)
    Behind,
    /// each side has unique commits
    Diverged,
    /// same commit
    Identical,
}

impl FromStr for CompareStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "ahead" => Self::Ahead,
            "behind" => Self::Behind,
            "diverged" => Self::Diverged,
            "identical" => Self::Identical,
            _ => return Err(()),
        })
    }
}

#[cfg(test)]
impl CompareStatus {
    pub const fn from_ancestry(
        base_is_ancestor_of_head: bool,
        head_is_ancestor_of_base: bool,
    ) -> Self {
        match (base_is_ancestor_of_head, head_is_ancestor_of_base) {
            (true, true) => Self::Identical,
            (true, false) => Self::Ahead,
            (false, true) => Self::Behind,
            (false, false) => Self::Diverged,
        }
    }
}

struct ResolvedGithubRef {
    rev:           String,
    last_modified: i64,
    comparison:    BranchComparison,
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

fn gh_ref_compare(
    owner: &str,
    repo: &str,
    reff: Option<&str>,
    old_rev: &str,
) -> FetchResult<ResolvedGithubRef> {
    let (query, variables) = reff.map_or_else(
        || {
            (
                GITHUB_DEFAULT_COMPARE_QUERY,
                json!({
                    "owner": owner,
                    "repo": repo,
                    "old": old_rev,
                }),
            )
        },
        |ref_name| {
            (
                GITHUB_REF_COMPARE_QUERY,
                json!({
                    "owner": owner,
                    "repo": repo,
                    "ref": ref_name,
                    "old": old_rev,
                }),
            )
        },
    );
    parse_gh_ref_compare(&gh_graphql(query, &variables)?)
}

fn parse_gh_ref_compare(parsed: &Value) -> FetchResult<ResolvedGithubRef> {
    let ref_node = parsed
        .get("data")
        .and_then(|data| data.get("repository"))
        .and_then(|repo| repo.get("targetRef"))
        .filter(|node| !node.is_null())
        .ok_or_else(|| FetchError::Upstream("github graphql response missing ref".to_owned()))?;
    let target = ref_node.get("target").ok_or_else(|| {
        FetchError::Upstream("github graphql response missing ref target".to_owned())
    })?;
    let (rev, last_modified) = target_commit(target)?;
    let comparison = ref_node
        .get("compare")
        .and_then(|compare| compare.get("status"))
        .and_then(Value::as_str)
        .and_then(graphql_ref_compare_status)
        .map_or_else(BranchComparison::unavailable, BranchComparison::verified);
    Ok(ResolvedGithubRef {
        rev,
        last_modified,
        comparison,
    })
}

/// when the GraphQL ref resolved but its comparison came back unavailable, the
/// REST `/compare` endpoint may still classify the two revs. a verified or
/// not-attempted comparison is left untouched.
fn backfill_comparison(
    owner: &str,
    repo: &str,
    old_rev: &str,
    resolved: ResolvedGithubRef,
) -> ResolvedGithubRef {
    let unavailable = resolved.comparison.status.is_none() && resolved.comparison.expected;
    if !unavailable || old_rev == resolved.rev {
        return resolved;
    }
    let comparison = compare_status(owner, repo, old_rev, &resolved.rev)
        .ok()
        .flatten()
        .map_or(resolved.comparison, BranchComparison::verified);
    ResolvedGithubRef {
        comparison,
        ..resolved
    }
}

fn target_commit(target: &Value) -> FetchResult<(String, i64)> {
    let commit = target
        .get("target")
        .filter(|inner| inner.get("committedDate").is_some())
        .unwrap_or(target);
    let rev = commit
        .get("oid")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FetchError::Upstream("github graphql response missing commit oid".to_owned())
        })?
        .to_owned();
    let date = commit
        .get("committedDate")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FetchError::Upstream("github graphql response missing commit date".to_owned())
        })?;
    let epoch = epoch_from_iso(date)
        .map_err(|err| FetchError::Upstream(format!("bad github commit date: {err}")))?;
    Ok((rev, epoch))
}

fn graphql_ref_compare_status(status: &str) -> Option<CompareStatus> {
    Some(match status {
        // Ref.compare compares `targetRef` as the base to the old locked rev
        // as the head. tack displays the current ref relative to the old rev.
        "AHEAD" => CompareStatus::Behind,
        "BEHIND" => CompareStatus::Ahead,
        "DIVERGED" => CompareStatus::Diverged,
        "IDENTICAL" => CompareStatus::Identical,
        _ => return None,
    })
}

/// compare `head` against `base` via github's compare endpoint. returns
/// [`None`] when the response carries no recognised `status`, which callers
/// treat as "no answer" and fall back to commit-date ordering
pub fn compare_status(
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
) -> FetchResult<Option<CompareStatus>> {
    let url = gh_compare_url(owner, repo, base, head);
    let parsed = gh_get_with_timeout(&url, Some(Duration::from_secs(5)))?;
    Ok(parsed
        .get("status")
        .and_then(Value::as_str)
        .and_then(|status| status.parse().ok()))
}

fn gh_compare_url(owner: &str, repo: &str, base: &str, head: &str) -> String {
    format!("https://api.github.com/repos/{owner}/{repo}/compare/{base}...{head}?per_page=1")
}

fn gh_commit(owner: &str, repo: &str, reff: &str) -> FetchResult<(String, i64)> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{reff}");
    let parsed = gh_get(&url)?;
    let rev = parsed["sha"]
        .as_str()
        .ok_or_else(|| {
            FetchError::Upstream(format!(
                "no sha in github response for {owner}/{repo}@{reff}"
            ))
        })?
        .to_owned();
    let date = parsed["commit"]["committer"]["date"]
        .as_str()
        .ok_or_else(|| FetchError::Upstream(format!("no commit date for {owner}/{repo}@{reff}")))?;
    let epoch = epoch_from_iso(date).map_err(|err| {
        FetchError::Upstream(format!("bad commit date for {owner}/{repo}@{reff}: {err}"))
    })?;
    Ok((rev, epoch))
}

pub struct CommitLog {
    /// freshest commits in the range, newest first
    pub fresh: Vec<(String, String)>,
    /// more than the requested limit existed; render an ellipsis
    pub more:  bool,
    /// the currently-pinned (base) commit, for context
    pub base:  Option<(String, String)>,
}

/// fresh commits between `old` and `new` revs, capped at `limit`. [`None`] for
/// non-github targets (no clone-free way to do this yet).
pub fn commits_between(
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
    let parsed = gh_get(&url)?;
    let commits = parsed["commits"].as_array().cloned().unwrap_or_default();
    let total = parsed
        .get("total_commits")
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(commits.len());
    let fresh = commits
        .iter()
        .rev()
        .take(limit)
        .filter_map(commit_pair)
        .collect::<Vec<_>>();
    let base = parsed.get("base_commit").and_then(commit_pair);
    Ok(Some(CommitLog {
        fresh,
        more: total > limit,
        base,
    }))
}

fn commit_pair(node: &Value) -> Option<(String, String)> {
    let sha = node.get("sha")?.as_str()?.to_owned();
    let msg = node.get("commit")?.get("message")?.as_str()?;
    let subject = msg.lines().next().unwrap_or("").trim_end().to_owned();
    Some((sha, subject))
}

/// get a text resource
pub fn raw(url: &str) -> FetchResult<String> {
    let mut resp = agent()
        .get(url)
        .header("User-Agent", "tack")
        .call()
        .map_err(|err| from_ureq(err, url))?;
    let status = resp.status();
    if status != 200 {
        return Err(http_error(status.as_u16(), url));
    }
    let mut body = String::new();
    resp.body_mut()
        .as_reader()
        .read_to_string(&mut body)
        .map_err(|err| {
            FetchError::Decode {
                what:   url.to_owned(),
                source: Box::new(err),
            }
        })?;
    Ok(body)
}

fn download_github_tarball(owner: &str, repo: &str, rev: &str, into: &Path) -> Result<PathBuf> {
    let url = format!("https://codeload.github.com/{owner}/{repo}/tar.gz/{rev}");
    let mut resp = agent()
        .get(&url)
        .header("User-Agent", "tack")
        .call()
        .wrap_err_with(|| format!("download {url}"))?;
    unpack_tar_stream(resp.body_mut().as_reader(), TarFormat::Gz, into)
}

/// check out `rev` (if given) or the tip of `reff` (or remote default) into
/// `into`; return (rev, time, refname)
fn git_checkout(
    url: &str,
    reff: Option<&str>,
    requested_rev: Option<&str>,
    submodules: bool,
    into: &Path,
) -> Result<(String, i64, String)> {
    let repo = Repository::init(into)?;
    let mut remote = repo.remote_anonymous(url)?;

    let refname = {
        let conn = remote.connect_auth(Direction::Fetch, Some(callbacks()), None)?;
        full_ref(reff, || branch_str(conn.default_branch()))
    };

    let mut fo = FetchOptions::new();
    fo.remote_callbacks(callbacks());
    // a specific rev can be anywhere in history, so fetch the ref in full;
    // for a moving ref we only need the tip
    if requested_rev.is_none() {
        fo.depth(1);
    }
    remote
        .fetch(&[&refname], Some(&mut fo), None)
        .wrap_err_with(|| format!("fetch {refname} from {url}"))?;

    let commit = match requested_rev {
        Some(pinned) => {
            repo.revparse_single(pinned)
                .wrap_err_with(|| format!("rev '{pinned}' not reachable from {refname} on {url}"))?
                .peel_to_commit()
                .wrap_err_with(|| format!("'{pinned}' is not a commit"))?
        },
        None => repo.find_reference("FETCH_HEAD")?.peel_to_commit()?,
    };
    let rev = commit.id().to_string();
    let time = commit.time().seconds();

    repo.checkout_tree(
        commit.tree()?.as_object(),
        Some(CheckoutBuilder::new().force()),
    )?;
    if submodules {
        update_submodules(&repo)?;
    }
    Ok((rev, time, refname))
}

fn update_submodules(repo: &Repository) -> Result<()> {
    for mut sm in repo.submodules()? {
        let mut opts = git2::SubmoduleUpdateOptions::new();
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(callbacks());
        opts.fetch(fo);
        sm.update(true, Some(&mut opts))?;
    }
    Ok(())
}

fn branch_str(raw: Result<git2::Buf, git2::Error>) -> Option<String> {
    let buf = raw.ok()?;
    buf.as_str().ok().map(str::to_owned)
}

fn full_ref(reff: Option<&str>, default: impl FnOnce() -> Option<String>) -> String {
    match reff {
        Some(target) if target.starts_with("refs/") => target.to_owned(),
        Some(target) => format!("refs/heads/{target}"),
        None => default().unwrap_or_else(|| "HEAD".to_owned()),
    }
}

fn callbacks() -> RemoteCallbacks<'static> {
    const NAMES: &[&str] = &["id_ed25519", "id_ecdsa", "id_rsa", "id_dsa"];

    let mut cb = RemoteCallbacks::new();
    let mut tried_agent = false;
    let mut key_idx = 0_usize;
    cb.credentials(move |_url, username, allowed| {
        let user = username.unwrap_or("git");
        if allowed.contains(CredentialType::USERNAME) {
            return Cred::username(user);
        }
        if !allowed.contains(CredentialType::SSH_KEY) {
            return Err(git2::Error::from_str("no supported credential type"));
        }
        if !tried_agent {
            tried_agent = true;
            if env::var_os("SSH_AUTH_SOCK").is_some()
                && let Ok(cred) = Cred::ssh_key_from_agent(user)
            {
                return Ok(cred);
            }
        }
        let ssh_dir = env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".ssh"))
            .ok_or_else(|| git2::Error::from_str("ssh: $HOME unset, cannot locate keys"))?;
        while key_idx < NAMES.len() {
            let path = ssh_dir.join(NAMES[key_idx]);
            key_idx += 1;
            if path.is_file() {
                return Cred::ssh_key(user, None, &path, None);
            }
        }
        Err(git2::Error::from_str(
            "ssh: no agent and no usable key under ~/.ssh",
        ))
    });
    cb
}

/// IMF-fixdate (e.g. `Sun, 06 Nov 1994 08:49:37 GMT`) to unix seconds.
fn epoch_from_http_date(input: &str) -> Result<i64> {
    let bytes = input.as_bytes();
    if bytes.len() < 29 {
        bail!("bad http date: {input}");
    }
    let slice = |range: Range<usize>| -> Result<&str> {
        input
            .get(range)
            .wrap_err_with(|| format!("bad http date: {input}"))
    };
    let parse_num = |range: Range<usize>| -> Result<i64> {
        slice(range)?
            .parse()
            .wrap_err_with(|| format!("bad http date: {input}"))
    };
    let day = parse_num(5..7)?;
    let month = match slice(8..11)? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        name => bail!("bad month in http date: {name}"),
    };
    let year = parse_num(12..16)?;
    let hh = parse_num(17..19)?;
    let mi = parse_num(20..22)?;
    let ss = parse_num(23..25)?;
    Ok(days_from_civil(year, month, day) * 86400 + hh * 3600 + mi * 60 + ss)
}

/// iso8601 to unix seconds
fn epoch_from_iso(input: &str) -> Result<i64> {
    let bytes = input.as_bytes();
    if bytes.len() < 20 {
        bail!("bad timestamp: {input}");
    }
    let parse_num = |range: Range<usize>| -> Result<i64> {
        input
            .get(range)
            .wrap_err_with(|| format!("bad timestamp: {input}"))?
            .parse()
            .wrap_err_with(|| format!("bad timestamp: {input}"))
    };
    let (year, month, day) = (parse_num(0..4)?, parse_num(5..7)?, parse_num(8..10)?);
    let (hh, mi, ss) = (parse_num(11..13)?, parse_num(14..16)?, parse_num(17..19)?);
    Ok(days_from_civil(year, month, day) * 86400 + hh * 3600 + mi * 60 + ss)
}

/// days since 1970-01-01 (howard hinnant)
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let yoe = adjusted_year - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_pinned_rev_skips_branch_comparison() {
        let source = "github:o/r?rev=abc123".parse::<Source>().unwrap();
        let mismatched = current_rev_compared(&source, Some("old")).unwrap();
        assert_eq!(mismatched.rev, "abc123");
        assert_eq!(mismatched.comparison, BranchComparison::none());

        let identical = current_rev_compared(&source, Some("abc123")).unwrap();
        assert_eq!(identical.rev, "abc123");
        assert_eq!(
            identical.comparison,
            BranchComparison::verified(CompareStatus::Identical)
        );
    }

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
        let parsed = json!({
            "data": {
                "repository": {
                    "targetRef": {
                        "target": {
                            "oid": "new",
                            "committedDate": "2026-05-30T18:08:13Z"
                        },
                        "compare": {
                            "status": "BEHIND",
                            "aheadBy": 0_i32,
                            "behindBy": 1_264_i32
                        }
                    }
                }
            }
        });

        let resolved = parse_gh_ref_compare(&parsed).unwrap();

        assert_eq!(resolved.rev, "new");
        assert_eq!(resolved.last_modified, 1_780_164_493);
        assert_eq!(
            resolved.comparison,
            BranchComparison::verified(CompareStatus::Ahead)
        );
    }

    #[test]
    fn parses_graphql_annotated_tag_target() {
        let parsed = json!({
            "data": {
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
            }
        });

        let resolved = parse_gh_ref_compare(&parsed).unwrap();

        assert_eq!(resolved.rev, "commit");
        assert_eq!(
            resolved.comparison,
            BranchComparison::verified(CompareStatus::Identical)
        );
    }

    #[test]
    fn rest_compare_url_limits_payload() {
        assert_eq!(
            gh_compare_url("o", "r", "base", "head"),
            "https://api.github.com/repos/o/r/compare/base...head?per_page=1"
        );
    }

    #[test]
    fn status_classification_separates_auth_absent_and_transport() {
        assert!(matches!(http_error(403, "x"), FetchError::Auth { .. }));
        assert!(matches!(http_error(401, "x"), FetchError::Auth { .. }));
        assert!(matches!(http_error(404, "x"), FetchError::NotFound { .. }));
        assert!(matches!(http_error(503, "x"), FetchError::Transport(_)));
    }

    // ureq surfaces non-2xx as Error::StatusCode, so the auth/not-found
    // classification must survive the error path, not only a returned response
    #[test]
    fn ureq_status_errors_classify_like_responses() {
        assert!(matches!(
            from_ureq(ureq::Error::StatusCode(403), "x"),
            FetchError::Auth { .. }
        ));
        assert!(matches!(
            from_ureq(ureq::Error::StatusCode(404), "x"),
            FetchError::NotFound { .. }
        ));
        assert!(matches!(
            from_ureq(ureq::Error::StatusCode(503), "x"),
            FetchError::Transport(_)
        ));
    }

    fn commit(repo: &Repository, parent_ids: &[git2::Oid], message: &str, time: i64) -> git2::Oid {
        let sig = git2::Signature::new("tack", "tack@example.invalid", &git2::Time::new(time, 0))
            .unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent_commits = parent_ids
            .iter()
            .map(|oid| repo.find_commit(*oid).unwrap())
            .collect::<Vec<_>>();
        let parent_refs = parent_commits.iter().collect::<Vec<_>>();
        repo.commit(None, &sig, &sig, message, &tree, &parent_refs)
            .unwrap()
    }

    fn local_compare(repo: &Repository, base: git2::Oid, head: git2::Oid) -> CompareStatus {
        if base == head {
            return CompareStatus::Identical;
        }
        let base_is_ancestor = repo.graph_descendant_of(head, base).unwrap();
        let head_is_ancestor = repo.graph_descendant_of(base, head).unwrap();
        CompareStatus::from_ancestry(base_is_ancestor, head_is_ancestor)
    }

    #[test]
    fn compare_status_from_local_merge_base_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
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

    #[test]
    fn tar_format_from_extension() {
        assert!(matches!(
            detect_tar_format("https://x/y.tar.xz").unwrap(),
            TarFormat::Xz
        ));
        assert!(matches!(
            detect_tar_format("https://x/y.txz").unwrap(),
            TarFormat::Xz
        ));
        assert!(matches!(
            detect_tar_format("https://x/y.tar.gz").unwrap(),
            TarFormat::Gz
        ));
        assert!(matches!(
            detect_tar_format("https://x/y.tgz").unwrap(),
            TarFormat::Gz
        ));
        assert!(matches!(
            detect_tar_format("https://x/y.tar").unwrap(),
            TarFormat::Plain
        ));
        // querystring and fragment must not defeat detection
        assert!(matches!(
            detect_tar_format("https://x/y.tar.xz?signed=1#frag").unwrap(),
            TarFormat::Xz
        ));
        assert!(detect_tar_format("https://x/y").is_err());
    }

    #[test]
    fn link_header_immutable() {
        let immutable = "<https://releases.nixos.org/nixos/abc/nixexprs.tar.xz>; rel=\"immutable\"";
        assert_eq!(
            parse_link_immutable(immutable).as_deref(),
            Some("https://releases.nixos.org/nixos/abc/nixexprs.tar.xz")
        );

        // rel=immutable_link is the historic name used by some nix releases
        let immutable_link = "<https://x/y>; rel=\"immutable_link\"";
        assert_eq!(
            parse_link_immutable(immutable_link).as_deref(),
            Some("https://x/y")
        );

        // a Link header without an immutable rel yields None, not the wrong URL
        let canonical = "<https://x/y>; rel=\"canonical\"";
        assert!(parse_link_immutable(canonical).is_none());

        // multiple values: the immutable one wins regardless of position
        let mixed = "<https://x/canon>; rel=\"canonical\", <https://x/imm>; rel=\"immutable\"";
        assert_eq!(
            parse_link_immutable(mixed).as_deref(),
            Some("https://x/imm")
        );
    }

    #[test]
    fn http_date_roundtrip() {
        // 1994-11-06T08:49:37Z = 784111777
        assert_eq!(
            epoch_from_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap(),
            784_111_777
        );
        let _ = epoch_from_http_date("bogus").unwrap_err();
        let _ = epoch_from_http_date("Sun, 06 Foo 1994 08:49:37 GMT").unwrap_err();
    }

    // our tarball nar hash must equal nix's narHash for this rev
    // cargo test -- --ignored
    #[test]
    #[ignore = "hits codeload.github.com"]
    fn github_narhash_matches_nix() {
        let dir = tempfile::tempdir().unwrap();
        let root = download_github_tarball(
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
