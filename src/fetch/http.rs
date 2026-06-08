// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::{
        BTreeSet,
        HashMap,
    },
    env,
    error::Error,
    fs,
    io::Read as _,
    mem,
    path::{
        Path,
        PathBuf,
    },
    result::Result as StdResult,
    sync::{
        Mutex,
        OnceLock,
        PoisonError,
    },
    time::{
        Duration,
        SystemTime,
        UNIX_EPOCH,
    },
};

use etcetera::BaseStrategy as _;
use serde::{
    Deserialize,
    Serialize,
    de::DeserializeOwned,
};
use ureq::{
    Agent,
    Body,
    RequestBuilder,
    http::{
        HeaderMap,
        Response,
        header::{
            ACCEPT,
            AUTHORIZATION,
            CONTENT_TYPE,
            USER_AGENT,
        },
    },
    tls::TlsConfig,
    typestate::{
        WithBody,
        WithoutBody,
    },
};

const TACK_USER_AGENT: &str = "tack";
const GITHUB_ACCEPT: &str = "application/vnd.github+json";
const GITHUB_GRAPHQL_URL: &str = "https://api.github.com/graphql";
const GITHUB_GRAPHQL_TIMEOUT: Duration = Duration::from_secs(15);
const APPLICATION_JSON: &str = "application/json";
const GITLAB_TOKEN_HEADER: &str = "PRIVATE-TOKEN";

pub(super) fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let config = TlsConfig::builder().build();
        Agent::config_builder().tls_config(config).build().into()
    })
}

fn token_for_host(host: &str) -> Option<&'static str> {
    let env_token = match host {
        "github.com" => github_env_token(),
        "gitlab.com" => gitlab_env_token(),
        _ => None,
    };
    env_token.or_else(|| nix_conf_tokens().get(host).map(String::as_str))
}

fn github_env_token() -> Option<&'static str> {
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN
        .get_or_init(|| non_empty_env("GITHUB_TOKEN").or_else(|| non_empty_env("GH_TOKEN")))
        .as_deref()
}

fn gitlab_env_token() -> Option<&'static str> {
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN
        .get_or_init(|| non_empty_env("GITLAB_TOKEN"))
        .as_deref()
}

/// an env var only when set to a non-empty value; an empty token is no token
fn non_empty_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.is_empty())
}

/// `host = token` pairs scraped from every nix.conf in the standard ladder,
/// merged with later files winning. empty unless opted into (see
/// [`nix_conf_scrape_enabled`])
fn nix_conf_tokens() -> &'static HashMap<String, String> {
    static TOKENS: OnceLock<HashMap<String, String>> = OnceLock::new();
    TOKENS.get_or_init(|| {
        let mut tokens = HashMap::new();
        if nix_conf_scrape_enabled() {
            for file in nix_conf_files() {
                scrape_access_tokens(&file, &mut tokens, 0);
            }
        }
        tokens
    })
}

/// scraping another tool's nix.conf for `access-tokens` and replaying them to a
/// forge is invasive, so it is opt-in via `TACK_NIX_CONF_TOKENS` (the NixOS
/// module exposes this as `programs.tack.nixConfTokens`)
fn nix_conf_scrape_enabled() -> bool {
    env_flag_enabled(env::var("TACK_NIX_CONF_TOKENS").ok().as_deref())
}

/// an on/off env flag: present and not an explicit falsey value
fn env_flag_enabled(value: Option<&str>) -> bool {
    matches!(
        value.map(str::to_ascii_lowercase).as_deref(),
        Some(flag) if !matches!(flag, "" | "0" | "false" | "no" | "off")
    )
}

/// nix.conf locations, lowest precedence first: system, then user, then the
/// explicit `NIX_USER_CONF_FILES` override list
fn nix_conf_files() -> Vec<PathBuf> {
    let mut files = vec![PathBuf::from("/etc/nix/nix.conf")];
    if let Ok(strategy) = etcetera::choose_base_strategy() {
        files.push(strategy.config_dir().join("nix/nix.conf"));
    }
    if let Some(list) = env::var_os("NIX_USER_CONF_FILES") {
        files.extend(env::split_paths(&list));
    }
    files
}

/// read `access-tokens` / `extra-access-tokens` out of one nix.conf, following
/// `!include` (bounded against include cycles)
fn scrape_access_tokens(path: &Path, tokens: &mut HashMap<String, String>, depth: u8) {
    const MAX_INCLUDE_DEPTH: u8 = 16;

    if depth > MAX_INCLUDE_DEPTH {
        return;
    }
    let Ok(contents) = fs::read_to_string(path) else {
        return;
    };
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("!include") {
            let target = rest.trim_start_matches('?').trim();
            if !target.is_empty() {
                let included = path
                    .parent()
                    .filter(|_| !Path::new(target).is_absolute())
                    .map_or_else(|| PathBuf::from(target), |base| base.join(target));
                scrape_access_tokens(&included, tokens, depth + 1);
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if matches!(key.trim(), "access-tokens" | "extra-access-tokens") {
            for pair in value.split_whitespace() {
                if let Some((host, token)) = pair.split_once('=')
                    && !host.is_empty()
                    && !token.is_empty()
                {
                    tokens.insert(host.to_owned(), token.to_owned());
                }
            }
        }
    }
}

fn token_warnings() -> &'static Mutex<BTreeSet<String>> {
    static WARNINGS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    WARNINGS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// record a "no token for this host" notice once; surfaced after the spinner
/// finishes so it does not corrupt the live display
fn record_token_warning(message: String) {
    token_warnings()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(message);
}

/// drain the deferred token warnings for a command to print after its display
pub fn drain_token_warnings() -> Vec<String> {
    let mut guard = token_warnings()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    mem::take(&mut *guard).into_iter().collect()
}

pub type FetchResult<T> = StdResult<T, FetchError>;

/// why a github query or raw-file probe failed; callers match to tolerate
/// degraded paths while surfacing fixable ones
#[derive(thiserror::Error, Debug)]
pub enum FetchError {
    #[error("{what} not found")]
    NotFound { what: String },

    #[error("auth: {what}")]
    Auth { what: String },

    #[error("rate limited: {what}")]
    RateLimited { what: String },

    /// unreachable host or transient server error
    #[error("network: {0}")]
    Transport(String),

    #[error("decoding {what}")]
    Decode {
        what:   String,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },

    /// response shape tack does not recognize
    #[error("unexpected github response: {0}")]
    Github(String),

    /// gitlab returned a response shape tack does not recognize
    #[error("unexpected gitlab response: {0}")]
    Gitlab(String),

    /// forgejo/gitea returned a response shape tack does not recognize
    #[error("unexpected forge response: {0}")]
    Forge(String),
}

impl FetchError {
    pub(super) fn from_status(status: u16, what: &str) -> Self {
        match status {
            401 | 403 => {
                Self::Auth {
                    what: what.to_owned(),
                }
            },
            404 => {
                Self::NotFound {
                    what: what.to_owned(),
                }
            },
            other => Self::Transport(format!("{what}: HTTP {other}")),
        }
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "ureq::Error is #[non_exhaustive]"
    )]
    pub(super) fn from_ureq(err: ureq::Error, what: &str) -> Self {
        match err {
            ureq::Error::StatusCode(code) => Self::from_status(code, what),
            other => Self::Transport(format!("{what}: {other}")),
        }
    }

    pub(super) fn from_response(resp: &mut Response<Body>, what: &str) -> Self {
        let status = resp.status().as_u16();
        if let Some(reason) = rate_limit_hint(resp.headers()) {
            return Self::RateLimited {
                what: format!("{what} ({reason})"),
            };
        }
        let labelled = read_api_error(resp)
            .map_or_else(|| what.to_owned(), |message| format!("{what}: {message}"));
        Self::from_status(status, &labelled)
    }

    const fn is_credential_failure(&self) -> bool {
        matches!(*self, Self::Auth { .. } | Self::RateLimited { .. })
    }
}

#[derive(Deserialize)]
struct ApiErrorBody {
    message:           Option<String>,
    error:             Option<String>,
    error_description: Option<String>,
}

impl ApiErrorBody {
    fn detail(self) -> Option<String> {
        let raw = self.message.or(self.error_description).or(self.error)?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(trimmed.chars().take(300).collect())
    }
}

fn read_api_error(resp: &mut Response<Body>) -> Option<String> {
    let mut body = String::new();
    resp.body_mut()
        .as_reader()
        .take(8 * 1024)
        .read_to_string(&mut body)
        .ok()?;
    serde_json::from_str::<ApiErrorBody>(&body)
        .ok()
        .and_then(ApiErrorBody::detail)
}

fn rate_limit_hint(headers: &HeaderMap) -> Option<String> {
    if let Some(secs) = header_value(headers, "retry-after") {
        return Some(format!("retry after {secs}s"));
    }
    if header_value(headers, "x-ratelimit-remaining").as_deref() == Some("0") {
        return Some(reset_hint(headers));
    }
    None
}

fn reset_hint(headers: &HeaderMap) -> String {
    let Some(reset) =
        header_value(headers, "x-ratelimit-reset").and_then(|raw| raw.parse::<u64>().ok())
    else {
        return "limit exhausted".to_owned();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    if reset > now {
        format!("resets in ~{}m", (reset - now).div_ceil(60))
    } else {
        "limit exhausted".to_owned()
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}

fn read_ok_body(resp: &mut Response<Body>, what: &str) -> FetchResult<String> {
    if resp.status() != 200 {
        return Err(FetchError::from_response(resp, what));
    }
    resp.body_mut()
        .read_to_string()
        .map_err(|err| FetchError::Transport(format!("read {what}: {err}")))
}

#[derive(Clone, Copy)]
enum Credential {
    Token(&'static str),
    Anonymous,
}

fn with_credential_fallback<T>(
    host: &str,
    allow_anon: bool,
    attempt: impl FnMut(Credential) -> FetchResult<T>,
) -> FetchResult<T> {
    run_credentials(token_for_host(host), host, allow_anon, attempt)
}

fn run_credentials<T>(
    token: Option<&'static str>,
    host: &str,
    allow_anon: bool,
    mut attempt: impl FnMut(Credential) -> FetchResult<T>,
) -> FetchResult<T> {
    let token_failure = match token {
        Some(value) => {
            match attempt(Credential::Token(value)) {
                Err(err) if err.is_credential_failure() => Some(err),
                result => return result,
            }
        },
        None => None,
    };
    if !allow_anon {
        return Err(token_failure.unwrap_or_else(|| {
            FetchError::Auth {
                what: format!("{host}: no usable credentials"),
            }
        }));
    }
    match attempt(Credential::Anonymous) {
        Err(anon_failure) => Err(most_actionable(token_failure, anon_failure)),
        result => result,
    }
}

fn most_actionable(token_failure: Option<FetchError>, anon_failure: FetchError) -> FetchError {
    match token_failure {
        Some(failure @ FetchError::Auth { .. }) => failure,
        _ => anon_failure,
    }
}

#[derive(Clone, Copy)]
pub(super) struct HttpClient {
    agent: &'static Agent,
}

impl HttpClient {
    pub(super) fn global() -> Self {
        Self { agent: agent() }
    }

    pub(super) fn get(self, url: &str) -> RequestBuilder<WithoutBody> {
        Self::with_tack_headers(self.agent.get(url))
    }

    pub(super) fn head(self, url: &str) -> RequestBuilder<WithoutBody> {
        Self::with_tack_headers(self.agent.head(url))
    }

    fn post(self, url: &str) -> RequestBuilder<WithBody> {
        Self::with_tack_headers(self.agent.post(url))
    }

    fn with_tack_headers<B>(request: RequestBuilder<B>) -> RequestBuilder<B> {
        request.header(USER_AGENT, TACK_USER_AGENT)
    }

    fn with_github_headers<B>(request: RequestBuilder<B>) -> RequestBuilder<B> {
        request.header(ACCEPT, GITHUB_ACCEPT)
    }

    fn with_json_body<B>(request: RequestBuilder<B>) -> RequestBuilder<B> {
        request.header(CONTENT_TYPE, APPLICATION_JSON)
    }

    fn with_github_credential<B>(
        request: RequestBuilder<B>,
        credential: Credential,
    ) -> RequestBuilder<B> {
        match credential {
            Credential::Token(token) => request.header(AUTHORIZATION, format!("Bearer {token}")),
            Credential::Anonymous => request,
        }
    }

    fn with_gitlab_credential<B>(
        request: RequestBuilder<B>,
        credential: Credential,
    ) -> RequestBuilder<B> {
        match credential {
            Credential::Token(token) => request.header(GITLAB_TOKEN_HEADER, token),
            Credential::Anonymous => request,
        }
    }

    pub(super) fn github_json<T>(self, url: &str, timeout_limit: Option<Duration>) -> FetchResult<T>
    where
        T: DeserializeOwned,
    {
        with_credential_fallback("github.com", true, |credential| {
            let mut req =
                Self::with_github_credential(Self::with_github_headers(self.get(url)), credential);
            if let Some(timeout) = timeout_limit {
                req = req.config().timeout_global(Some(timeout)).build();
            }
            let mut resp = req.call().map_err(|err| FetchError::from_ureq(err, url))?;
            let body = read_ok_body(&mut resp, url)?;
            serde_json::from_str::<T>(&body)
                .map_err(|err| FetchError::Github(format!("api {url}: invalid json: {err}")))
        })
    }

    pub(super) fn gitlab_json<T>(
        self,
        url: &str,
        host: &str,
        timeout_limit: Option<Duration>,
    ) -> FetchResult<T>
    where
        T: DeserializeOwned,
    {
        if token_for_host(host).is_none() {
            record_token_warning(format!(
                "no access token for {host}; gitlab comparison may be rate-limited or unavailable \
                 for private projects (set GITLAB_TOKEN, or opt into nix.conf access-tokens with \
                 TACK_NIX_CONF_TOKENS=1)"
            ));
        }
        with_credential_fallback(host, true, |credential| {
            let mut req = Self::with_gitlab_credential(
                self.get(url).header(ACCEPT, APPLICATION_JSON),
                credential,
            );
            if let Some(timeout) = timeout_limit {
                req = req.config().timeout_global(Some(timeout)).build();
            }
            let mut resp = req.call().map_err(|err| FetchError::from_ureq(err, url))?;
            let body = read_ok_body(&mut resp, url)?;
            serde_json::from_str::<T>(&body)
                .map_err(|err| FetchError::Gitlab(format!("api {url}: invalid json: {err}")))
        })
    }

    pub(super) fn github_graphql<V, T>(self, query: &str, variables: &V) -> FetchResult<T>
    where
        V: Serialize,
        T: DeserializeOwned,
    {
        if token_for_host("github.com").is_none() {
            record_token_warning(
                "no GitHub token; rev comparison falls back to commit lookup plus git DAG compare \
                 (set GITHUB_TOKEN or GH_TOKEN, or opt into nix.conf access-tokens with \
                 TACK_NIX_CONF_TOKENS=1)"
                    .to_owned(),
            );
        }

        let payload = serde_json::to_string(&GraphqlRequest { query, variables })
            .map_err(|err| FetchError::Github(format!("serialize graphql request: {err}")))?;

        with_credential_fallback("github.com", false, |credential| {
            let mut resp = Self::with_github_credential(
                Self::with_json_body(Self::with_github_headers(self.post(GITHUB_GRAPHQL_URL))),
                credential,
            )
            .config()
            .timeout_global(Some(GITHUB_GRAPHQL_TIMEOUT))
            .build()
            .send(payload.as_str())
            .map_err(|err| FetchError::from_ureq(err, "github graphql"))?;

            let body = read_ok_body(&mut resp, "github graphql")?;

            let parsed = serde_json::from_str::<GraphqlResponse<T>>(&body)
                .map_err(|err| FetchError::Github(format!("graphql invalid json: {err}")))?;

            if let Some(error) = parsed.errors.first() {
                let message = error.message();
                if error.is_auth() {
                    return Err(FetchError::Auth {
                        what: message.to_owned(),
                    });
                }
                return Err(FetchError::Github(format!("graphql: {message}")));
            }

            parsed
                .data
                .ok_or_else(|| FetchError::Github("graphql response missing data".to_owned()))
        })
    }

    pub(super) fn raw_text(self, url: &str) -> FetchResult<String> {
        let mut resp = self
            .get(url)
            .call()
            .map_err(|err| FetchError::from_ureq(err, url))?;
        read_ok_body(&mut resp, url)
    }
}

#[derive(Serialize)]
struct GraphqlRequest<'a, V> {
    query:     &'a str,
    variables: &'a V,
}

#[derive(Deserialize)]
struct GraphqlResponse<T> {
    data:   Option<T>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Deserialize)]
struct GraphqlError {
    #[serde(default)]
    message: String,
    #[serde(rename = "type")]
    kind:    Option<String>,
}

impl GraphqlError {
    fn message(&self) -> &str {
        if self.message.is_empty() {
            "github graphql error"
        } else {
            &self.message
        }
    }

    fn is_auth(&self) -> bool {
        self.kind
            .as_deref()
            .is_some_and(|kind| matches!(kind, "FORBIDDEN" | "UNAUTHORIZED"))
            || {
                let lower = self.message.to_ascii_lowercase();
                lower.contains("bad credentials")
                    || lower.contains("forbidden")
                    || lower.contains("unauthorized")
            }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
    };

    use ureq::http::{
        HeaderMap,
        HeaderName,
    };

    use super::{
        ApiErrorBody,
        Credential,
        FetchError,
        FetchResult,
        env_flag_enabled,
        rate_limit_hint,
        run_credentials,
        scrape_access_tokens,
    };

    fn label(credential: Credential) -> &'static str {
        match credential {
            Credential::Token(_) => "token",
            Credential::Anonymous => "anon",
        }
    }

    fn body(json: &str) -> Option<String> {
        serde_json::from_str::<ApiErrorBody>(json).unwrap().detail()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for &(name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn token_failure_falls_through_to_anon() {
        let mut tried = Vec::new();
        let auth = run_credentials(Some("t"), "h", true, |credential| {
            tried.push(label(credential));
            match credential {
                Credential::Token(_) => {
                    Err(FetchError::Auth {
                        what: "rejected".to_owned(),
                    })
                },
                Credential::Anonymous => Ok(1_i32),
            }
        });
        assert_eq!(auth.unwrap(), 1_i32);
        assert_eq!(tried, vec!["token", "anon"]);

        let limited: FetchResult<i32> = run_credentials(Some("t"), "h", true, |credential| {
            match credential {
                Credential::Token(_) => {
                    Err(FetchError::RateLimited {
                        what: "limited".to_owned(),
                    })
                },
                Credential::Anonymous => Ok(2_i32),
            }
        });
        assert_eq!(limited.unwrap(), 2_i32);
    }

    #[test]
    fn anon_used_directly_when_no_token() {
        let mut tried = Vec::new();
        let result = run_credentials(None, "h", true, |credential| {
            tried.push(label(credential));
            Ok::<_, FetchError>(9_i32)
        });
        assert_eq!(result.unwrap(), 9_i32);
        assert_eq!(tried, vec!["anon"]);
    }

    #[test]
    fn non_credential_token_error_stops_before_anon() {
        let mut count = 0_u8;
        let result: FetchResult<i32> = run_credentials(Some("t"), "h", true, |_| {
            count += 1;
            Err(FetchError::Transport("boom".to_owned()))
        });
        assert!(matches!(result, Err(FetchError::Transport(_))));
        assert_eq!(count, 1_u8);
    }

    #[test]
    fn token_auth_outranks_a_transient_anon_rate_limit() {
        let result: FetchResult<i32> = run_credentials(Some("t"), "h", true, |credential| {
            match credential {
                Credential::Token(_) => {
                    Err(FetchError::Auth {
                        what: "rejected".to_owned(),
                    })
                },
                Credential::Anonymous => {
                    Err(FetchError::RateLimited {
                        what: "limited".to_owned(),
                    })
                },
            }
        });
        assert!(matches!(result, Err(FetchError::Auth { .. })));
    }

    #[test]
    fn no_anon_rung_surfaces_token_auth_or_no_credentials() {
        let rejected: FetchResult<i32> = run_credentials(Some("t"), "h", false, |_| {
            Err(FetchError::Auth {
                what: "rejected".to_owned(),
            })
        });
        assert!(matches!(rejected, Err(FetchError::Auth { .. })));

        let none: FetchResult<i32> = run_credentials(None, "h", false, |_| Ok(1_i32));
        assert!(matches!(none, Err(FetchError::Auth { .. })));
    }

    #[test]
    fn api_error_detail_prefers_message_then_description_then_error() {
        assert_eq!(body(r#"{"message":"nope"}"#).as_deref(), Some("nope"));
        assert_eq!(
            body(r#"{"error":"e","error_description":"d"}"#).as_deref(),
            Some("d")
        );
        assert_eq!(
            body(r#"{"error":"bad token"}"#).as_deref(),
            Some("bad token")
        );
        assert_eq!(body(r#"{"message":"   "}"#), None);
        assert_eq!(body("{}"), None);
    }

    #[test]
    fn rate_limit_hint_reads_retry_after_and_exhausted_remaining() {
        assert_eq!(
            rate_limit_hint(&headers(&[("retry-after", "30")])).as_deref(),
            Some("retry after 30s")
        );
        assert!(
            rate_limit_hint(&headers(&[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "99999999999"),
            ]))
            .is_some_and(|hint| hint.starts_with("resets in ~"))
        );
        assert!(rate_limit_hint(&headers(&[("x-ratelimit-remaining", "42")])).is_none());
        assert!(rate_limit_hint(&headers(&[])).is_none());
    }

    #[test]
    fn env_flag_is_on_only_for_truthy_values() {
        assert!(env_flag_enabled(Some("1")));
        assert!(env_flag_enabled(Some("true")));
        assert!(env_flag_enabled(Some("YES")));
        assert!(!env_flag_enabled(None));
        assert!(!env_flag_enabled(Some("")));
        assert!(!env_flag_enabled(Some("0")));
        assert!(!env_flag_enabled(Some("false")));
        assert!(!env_flag_enabled(Some("off")));
    }

    #[test]
    fn access_tokens_scrape_follows_include_and_lets_later_lines_win() {
        let dir = tempfile::tempdir().unwrap();
        let included = dir.path().join("extra.conf");
        fs::write(&included, "access-tokens = gitlab.example.com=inc\n").unwrap();
        let main = dir.path().join("nix.conf");
        fs::write(
            &main,
            "# a comment\naccess-tokens = github.com=gh gitlab.com=gl\nextra-access-tokens = \
             gitlab.com=override\n!include extra.conf\n",
        )
        .unwrap();

        let mut tokens = HashMap::new();
        scrape_access_tokens(&main, &mut tokens, 0);

        assert_eq!(tokens.get("github.com").map(String::as_str), Some("gh"));
        // a later line (extra-access-tokens) overrides the earlier value
        assert_eq!(
            tokens.get("gitlab.com").map(String::as_str),
            Some("override")
        );
        // a `!include`d file is followed, relative to the includer
        assert_eq!(
            tokens.get("gitlab.example.com").map(String::as_str),
            Some("inc")
        );
    }

    #[test]
    fn status_classification_separates_auth_absent_and_transport() {
        assert!(matches!(
            FetchError::from_status(403, "x"),
            FetchError::Auth { .. }
        ));
        assert!(matches!(
            FetchError::from_status(401, "x"),
            FetchError::Auth { .. }
        ));
        assert!(matches!(
            FetchError::from_status(404, "x"),
            FetchError::NotFound { .. }
        ));
        assert!(matches!(
            FetchError::from_status(503, "x"),
            FetchError::Transport(_)
        ));
        assert_eq!(FetchError::from_status(403, "x").to_string(), "auth: x");
    }

    // ureq surfaces non-2xx as Error::StatusCode; classification must survive
    // the error path too
    #[test]
    fn ureq_status_errors_classify_like_responses() {
        assert!(matches!(
            FetchError::from_ureq(ureq::Error::StatusCode(403), "x"),
            FetchError::Auth { .. }
        ));
        assert!(matches!(
            FetchError::from_ureq(ureq::Error::StatusCode(404), "x"),
            FetchError::NotFound { .. }
        ));
        assert!(matches!(
            FetchError::from_ureq(ureq::Error::StatusCode(503), "x"),
            FetchError::Transport(_)
        ));
    }
}
