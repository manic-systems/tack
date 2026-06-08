// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    collections::HashMap,
    fs,
    io::Read as _,
    panic,
    path::PathBuf,
    sync::{
        Mutex,
        OnceLock,
        PoisonError,
    },
    thread,
    time::{
        Duration,
        SystemTime,
        UNIX_EPOCH,
    },
};

use etcetera::{
    BaseStrategy as _,
    choose_base_strategy,
};
use serde::{
    Deserialize,
    Serialize,
};
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
use crate::{
    project::write_atomic,
    source::git_url,
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const COMPARE_TIMEOUT: Duration = Duration::from_secs(5);
const APPLICATION_JSON: &str = "application/json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

    let (ahead_count, behind_count) = thread::scope(|scope| {
        let ahead = scope.spawn(|| compare_count(host, owner, repo, base, head));
        let behind = compare_count(host, owner, repo, head, base);
        (ahead.join(), behind)
    });
    let ahead = ahead_count.unwrap_or_else(|payload| panic::resume_unwind(payload))? > 0;
    let behind = behind_count? > 0;
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

const DETECTION_TTL_DAYS: u64 = 14;
const DETECTION_TTL: Duration = Duration::from_secs(DETECTION_TTL_DAYS * 24 * 60 * 60);
const DETECTION_FILE: &str = "forge-detection.json";

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

    if let Some(detected) = disk_lookup(host) {
        cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(host.to_owned(), detected);
        return detected;
    }

    let probe = probe_host(host);
    if probe.cacheable {
        cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(host.to_owned(), probe.kind);
        disk_store(host, probe.kind);
    }
    probe.kind
}

fn host_cache() -> &'static Mutex<HashMap<String, ForgeKind>> {
    static CACHE: OnceLock<Mutex<HashMap<String, ForgeKind>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// the result of probing a host: the detected kind plus whether it is safe to
/// persist. forge kinds are always cacheable; `Unknown` only when every probe
/// reached the host and it simply is not a forge — a transport blip leaves it
/// uncacheable so a brief outage cannot pin a host as "not a forge" until TTL
struct ProbeResult {
    kind:      ForgeKind,
    cacheable: bool,
}

fn probe_host(host: &str) -> ProbeResult {
    let mut reachable = true;
    match probe_version(&format!("https://{host}/api/forgejo/v1/version")) {
        ProbeHit::Matched => return ProbeResult::found(ForgeKind::Forgejo),
        ProbeHit::Missing => {},
        ProbeHit::Unreachable => reachable = false,
    }
    match probe_version(&format!("https://{host}/api/v1/version")) {
        ProbeHit::Matched => return ProbeResult::found(ForgeKind::Gitea),
        ProbeHit::Missing => {},
        ProbeHit::Unreachable => reachable = false,
    }
    for endpoint in ["metadata", "version"] {
        match probe_gitlab(host, endpoint) {
            ProbeHit::Matched => return ProbeResult::found(ForgeKind::Gitlab),
            ProbeHit::Missing => {},
            ProbeHit::Unreachable => reachable = false,
        }
    }
    match probe_cgit(host) {
        ProbeHit::Matched => return ProbeResult::found(ForgeKind::Cgit),
        ProbeHit::Missing => {},
        ProbeHit::Unreachable => reachable = false,
    }
    ProbeResult::unknown(reachable)
}

impl ProbeResult {
    const fn found(kind: ForgeKind) -> Self {
        Self {
            kind,
            cacheable: true,
        }
    }

    /// `Unknown` is only cacheable when every probe reached the host; a
    /// transport blip leaves it uncacheable so it is re-probed next run
    const fn unknown(reachable: bool) -> Self {
        Self {
            kind:      ForgeKind::Unknown,
            cacheable: reachable,
        }
    }
}

/// a single probe's outcome: a positive match, a definitive miss (the host
/// answered but is not this forge), or a transport failure (could not tell)
enum ProbeHit {
    Matched,
    Missing,
    Unreachable,
}

impl ProbeHit {
    const fn from_match(matched: bool) -> Self {
        if matched {
            Self::Matched
        } else {
            Self::Missing
        }
    }
}

fn probe_version(url: &str) -> ProbeHit {
    match json::<VersionResponse>(url, PROBE_TIMEOUT) {
        Ok(version) => ProbeHit::from_match(version.version.is_some_and(|value| !value.is_empty())),
        Err(err) if is_definitive(&err) => ProbeHit::Missing,
        Err(_) => ProbeHit::Unreachable,
    }
}

fn probe_gitlab(host: &str, endpoint: &str) -> ProbeHit {
    let url = format!("https://{host}/api/v4/{endpoint}");
    let resp = match get_text(&url, PROBE_TIMEOUT, 1024) {
        Ok(resp) => resp,
        Err(err) if is_definitive(&err) => return ProbeHit::Missing,
        Err(_) => return ProbeHit::Unreachable,
    };
    if has_gitlab_header(&resp.headers) {
        return ProbeHit::Matched;
    }
    ProbeHit::from_match(
        resp.status == 200
            && serde_json::from_str::<GitlabProbe>(&resp.body)
                .ok()
                .is_some_and(|probe| probe.version.is_some() || probe.revision.is_some()),
    )
}

fn probe_cgit(host: &str) -> ProbeHit {
    let url = format!("https://{host}/");
    let resp = match get_text(&url, PROBE_TIMEOUT, 16 * 1024) {
        Ok(resp) => resp,
        Err(err) if is_definitive(&err) => return ProbeHit::Missing,
        Err(_) => return ProbeHit::Unreachable,
    };
    if resp.status != 200 {
        return ProbeHit::Missing;
    }
    let body = resp.body.to_ascii_lowercase();
    ProbeHit::from_match(
        body.contains("name='generator' content='cgit")
            || body.contains("name=\"generator\" content=\"cgit")
            || body.contains("<div id='cgit'")
            || body.contains("<div id=\"cgit\""),
    )
}

/// whether an error means the host answered (so absence of the forge is
/// definitive) rather than that the request never completed
const fn is_definitive(err: &FetchError) -> bool {
    !matches!(err, FetchError::Transport(_))
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

#[derive(Clone, Copy, Serialize, Deserialize)]
struct DiskEntry {
    kind:      ForgeKind,
    probed_at: u64,
}

fn detection_cache_path() -> Option<PathBuf> {
    let strategy = choose_base_strategy().ok()?;
    Some(strategy.cache_dir().join("tack").join(DETECTION_FILE))
}

fn disk_lookup(host: &str) -> Option<ForgeKind> {
    let path = detection_cache_path()?;
    let contents = fs::read_to_string(&path).ok()?;
    let entries = serde_json::from_str::<HashMap<String, DiskEntry>>(&contents).ok()?;
    let entry = entries.get(host)?;
    is_fresh(entry, now_unix()).then_some(entry.kind)
}

fn disk_store(host: &str, kind: ForgeKind) {
    let Some(path) = detection_cache_path() else {
        return;
    };
    let mut entries = fs::read_to_string(&path)
        .ok()
        .and_then(|contents| serde_json::from_str::<HashMap<String, DiskEntry>>(&contents).ok())
        .unwrap_or_default();
    entries.insert(host.to_owned(), DiskEntry {
        kind,
        probed_at: now_unix(),
    });
    let Ok(mut json) = serde_json::to_string_pretty(&entries) else {
        return;
    };
    json.push('\n');
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = write_atomic(&path, &json);
}

/// an entry is usable while its probe timestamp sits within the TTL window; a
/// future timestamp (clock skewed backwards since the write) still counts fresh
fn is_fresh(entry: &DiskEntry, now: u64) -> bool {
    now.checked_sub(entry.probed_at)
        .is_none_or(|age| age <= DETECTION_TTL.as_secs())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        DETECTION_TTL,
        DiskEntry,
        FetchError,
        ForgeKind,
        ProbeResult,
        is_definitive,
        is_fresh,
    };

    fn entry(probed_at: u64) -> DiskEntry {
        DiskEntry {
            kind: ForgeKind::Gitea,
            probed_at,
        }
    }

    #[test]
    fn freshness_tracks_the_ttl_window() {
        let ttl = DETECTION_TTL.as_secs();
        let now = 2_000_000_000;
        assert!(is_fresh(&entry(now), now));
        assert!(is_fresh(&entry(now - ttl), now));
        assert!(!is_fresh(&entry(now - ttl - 1), now));
        // a clock skewed backwards must not read as infinitely fresh
        assert!(is_fresh(&entry(now + 5), now));
    }

    #[test]
    fn forge_kinds_are_always_cacheable() {
        assert!(ProbeResult::found(ForgeKind::Forgejo).cacheable);
        assert!(ProbeResult::found(ForgeKind::Cgit).cacheable);
    }

    #[test]
    fn unknown_caches_only_when_every_probe_reached_the_host() {
        let definitive = ProbeResult::unknown(true);
        assert_eq!(definitive.kind, ForgeKind::Unknown);
        assert!(definitive.cacheable);

        let inconclusive = ProbeResult::unknown(false);
        assert_eq!(inconclusive.kind, ForgeKind::Unknown);
        assert!(!inconclusive.cacheable);
    }

    #[test]
    fn transport_errors_are_inconclusive_others_definitive() {
        assert!(!is_definitive(&FetchError::Transport("dropped".to_owned())));
        assert!(is_definitive(&FetchError::NotFound {
            what: "x".to_owned(),
        }));
        assert!(is_definitive(&FetchError::Auth {
            what: "x".to_owned(),
        }));
        assert!(is_definitive(&FetchError::Forge("bad json".to_owned())));
    }

    #[test]
    fn disk_entries_round_trip_through_json() {
        let mut entries = HashMap::new();
        entries.insert("git.example.org".to_owned(), entry(42));
        entries.insert("unknown.example".to_owned(), DiskEntry {
            kind:      ForgeKind::Unknown,
            probed_at: 7,
        });
        let json = serde_json::to_string(&entries).unwrap();
        let parsed = serde_json::from_str::<HashMap<String, DiskEntry>>(&json).unwrap();
        assert_eq!(parsed["git.example.org"].kind, ForgeKind::Gitea);
        assert_eq!(parsed["git.example.org"].probed_at, 42);
        assert_eq!(parsed["unknown.example"].kind, ForgeKind::Unknown);
    }
}
