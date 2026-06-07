// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    collections::HashMap,
    io::Read as _,
    sync::{
        Mutex,
        OnceLock,
        PoisonError,
    },
    time::Duration,
};

use serde::Deserialize;
use ureq::http::{
    HeaderMap,
    header::ACCEPT,
};

use super::{
    CompareStatus,
    gitlab,
    http::{
        FetchError,
        FetchResult,
        HttpClient,
    },
};
use crate::source::git_url;

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const COMPARE_TIMEOUT: Duration = Duration::from_secs(5);
const APPLICATION_JSON: &str = "application/json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForgeKind {
    Gitlab,
    Forgejo,
    Gitea,
    Cgit,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedRepo {
    pub kind:  ForgeKind,
    pub host:  String,
    pub owner: String,
    pub repo:  String,
}

pub fn detect_git_url(url: &str) -> Option<HostedRepo> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return None;
    }
    let repo = git_url::parse(url)?;
    let kind = detect_host(&repo.host);
    Some(HostedRepo {
        kind,
        host: repo.host,
        owner: repo.owner,
        repo: repo.repo,
    })
}

pub fn compare_status(
    kind: ForgeKind,
    host: &str,
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
) -> FetchResult<Option<CompareStatus>> {
    compare_detected(kind, host, owner, repo, base, head)
}

pub fn resolve_ref(
    kind: ForgeKind,
    host: &str,
    owner: &str,
    repo: &str,
    reff: Option<&str>,
) -> FetchResult<Option<String>> {
    match kind {
        ForgeKind::Forgejo | ForgeKind::Gitea => {
            resolve_forgejo_gitea(host, owner, repo, reff).map(Some)
        },
        ForgeKind::Gitlab | ForgeKind::Cgit | ForgeKind::Unknown => Ok(None),
    }
}

fn resolve_forgejo_gitea(
    host: &str,
    owner: &str,
    repo: &str,
    reff: Option<&str>,
) -> FetchResult<String> {
    let mut url = format!(
        "{}/commits?limit=1&stat=false&verification=false&files=false",
        forgejo_repo_api(host, owner, repo)
    );
    if let Some(target) = reff {
        url.push_str("&sha=");
        url.push_str(&percent_encode(target));
    }
    json::<Vec<ForgeCommitRef>>(&url, COMPARE_TIMEOUT)?
        .into_iter()
        .next()
        .map(|commit| commit.sha)
        .ok_or_else(|| FetchError::Forge(format!("no commits resolving {owner}/{repo}")))
}

fn forgejo_repo_api(host: &str, owner: &str, repo: &str) -> String {
    let encoded_owner = owner
        .split('/')
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("/");
    format!(
        "https://{host}/api/v1/repos/{encoded_owner}/{}",
        percent_encode(repo),
    )
}

fn compare_detected(
    kind: ForgeKind,
    host: &str,
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
) -> FetchResult<Option<CompareStatus>> {
    Ok(match kind {
        ForgeKind::Gitlab => gitlab::compare_status(host, owner, repo, base, head)?,
        ForgeKind::Forgejo | ForgeKind::Gitea => {
            Some(compare_forgejo_gitea(host, owner, repo, base, head)?)
        },
        ForgeKind::Cgit | ForgeKind::Unknown => None,
    })
}

fn compare_forgejo_gitea(
    host: &str,
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
) -> FetchResult<CompareStatus> {
    if base == head {
        return Ok(CompareStatus::Identical);
    }

    let ahead = compare_count(host, owner, repo, base, head)? > 0;
    let behind = compare_count(host, owner, repo, head, base)? > 0;
    Ok(match (ahead, behind) {
        (false, false) => CompareStatus::Identical,
        (true, false) => CompareStatus::Ahead,
        (false, true) => CompareStatus::Behind,
        (true, true) => CompareStatus::Diverged,
    })
}

fn compare_count(host: &str, owner: &str, repo: &str, base: &str, head: &str) -> FetchResult<u64> {
    let url = format!(
        "{}/compare/{}...{}",
        forgejo_repo_api(host, owner, repo),
        percent_encode(base),
        percent_encode(head),
    );
    let parsed = json::<ForgeCompare>(&url, COMPARE_TIMEOUT)?;
    Ok(parsed.total_commits)
}

fn detect_host(host: &str) -> ForgeKind {
    let cache = host_cache();
    {
        let cached = cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(host)
            .copied();
        if let Some(detected) = cached {
            return detected;
        }
    }

    let detected = probe_host(host);
    if detected != ForgeKind::Unknown {
        cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(host.to_owned(), detected);
    }
    detected
}

fn host_cache() -> &'static Mutex<HashMap<String, ForgeKind>> {
    static CACHE: OnceLock<Mutex<HashMap<String, ForgeKind>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn probe_host(host: &str) -> ForgeKind {
    if probe_version(&format!("https://{host}/api/forgejo/v1/version")) {
        return ForgeKind::Forgejo;
    }
    if probe_version(&format!("https://{host}/api/v1/version")) {
        return ForgeKind::Gitea;
    }
    if probe_gitlab(host, "metadata") || probe_gitlab(host, "version") {
        return ForgeKind::Gitlab;
    }
    if probe_cgit(host) {
        return ForgeKind::Cgit;
    }
    ForgeKind::Unknown
}

fn probe_version(url: &str) -> bool {
    json::<VersionResponse>(url, PROBE_TIMEOUT)
        .ok()
        .and_then(|version| version.version)
        .is_some_and(|version| !version.is_empty())
}

fn probe_gitlab(host: &str, endpoint: &str) -> bool {
    let url = format!("https://{host}/api/v4/{endpoint}");
    let Ok(resp) = get_text(&url, PROBE_TIMEOUT, 1024) else {
        return false;
    };
    if has_gitlab_header(&resp.headers) {
        return true;
    }
    resp.status == 200
        && serde_json::from_str::<GitlabProbe>(&resp.body)
            .ok()
            .is_some_and(|probe| probe.version.is_some() || probe.revision.is_some())
}

fn probe_cgit(host: &str) -> bool {
    let url = format!("https://{host}/");
    let Ok(resp) = get_text(&url, PROBE_TIMEOUT, 16 * 1024) else {
        return false;
    };
    if resp.status != 200 {
        return false;
    }
    let body = resp.body.to_ascii_lowercase();
    body.contains("name='generator' content='cgit")
        || body.contains("name=\"generator\" content=\"cgit")
        || body.contains("<div id='cgit'")
        || body.contains("<div id=\"cgit\"")
}

fn has_gitlab_header(headers: &HeaderMap) -> bool {
    headers.contains_key("x-gitlab-meta")
        || headers.contains_key("gitlab-lb")
        || headers.contains_key("gitlab-sv")
}

fn json<T>(url: &str, timeout: Duration) -> FetchResult<T>
where
    T: for<'de> Deserialize<'de>,
{
    let mut resp = HttpClient::global()
        .get(url)
        .header(ACCEPT, APPLICATION_JSON)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
        .map_err(|err| FetchError::from_ureq(err, url))?;
    let status = resp.status();
    if status != 200 {
        return Err(FetchError::from_status(status.as_u16(), url));
    }
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|err| FetchError::Transport(format!("read forge api {url}: {err}")))?;
    serde_json::from_str::<T>(&body)
        .map_err(|err| FetchError::Forge(format!("api {url}: invalid json: {err}")))
}

fn get_text(url: &str, timeout: Duration, limit: u64) -> FetchResult<ProbeResponse> {
    let mut resp = HttpClient::global()
        .get(url)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
        .map_err(|err| FetchError::from_ureq(err, url))?;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let mut body = String::new();
    resp.body_mut()
        .as_reader()
        .take(limit)
        .read_to_string(&mut body)
        .map_err(|err| FetchError::Transport(format!("read forge probe {url}: {err}")))?;
    Ok(ProbeResponse {
        status,
        headers,
        body,
    })
}

fn percent_encode(value: &str) -> Cow<'_, str> {
    percent_encoding::percent_encode(value.as_bytes(), super::PERCENT_ENCODE_SET).into()
}

struct ProbeResponse {
    status:  u16,
    headers: HeaderMap,
    body:    String,
}

#[derive(Deserialize)]
struct VersionResponse {
    version: Option<String>,
}

#[derive(Deserialize)]
struct GitlabProbe {
    version:  Option<String>,
    revision: Option<String>,
}

#[derive(Deserialize)]
struct ForgeCompare {
    total_commits: u64,
}

#[derive(Deserialize)]
struct ForgeCommitRef {
    sha: String,
}
