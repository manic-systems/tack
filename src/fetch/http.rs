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
    time::Duration,
};

use etcetera::BaseStrategy as _;
use serde::{
    Deserialize,
    Serialize,
    de::DeserializeOwned,
};
use ureq::{
    Agent,
    RequestBuilder,
    http::header::{
        ACCEPT,
        AUTHORIZATION,
        CONTENT_TYPE,
        USER_AGENT,
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

/// an access token for `host`, from the environment first (the well-known
/// public forges) then nix.conf `access-tokens`; self-hosted forges resolve
/// only through nix.conf
fn token_for_host(host: &str) -> Option<&'static str> {
    if host == "github.com"
        && let Some(token) = github_env_token()
    {
        return Some(token);
    }
    if host == "gitlab.com"
        && let Some(token) = gitlab_env_token()
    {
        return Some(token);
    }
    nix_conf_tokens().get(host).map(String::as_str)
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
pub(in crate::fetch) fn record_token_warning(message: String) {
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

    fn github_get(self, url: &str) -> RequestBuilder<WithoutBody> {
        Self::with_optional_github_auth(Self::with_github_headers(self.get(url)))
    }

    fn github_post(self, url: &str, token: &str) -> RequestBuilder<WithBody> {
        let github_request = Self::with_github_headers(self.post(url));
        let json_request = Self::with_json_body(github_request);
        Self::with_github_auth(json_request, token)
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

    fn with_optional_github_auth<B>(request: RequestBuilder<B>) -> RequestBuilder<B> {
        if let Some(token) = token_for_host("github.com") {
            Self::with_github_auth(request, token)
        } else {
            request
        }
    }

    fn with_optional_gitlab_auth<B>(request: RequestBuilder<B>, host: &str) -> RequestBuilder<B> {
        if let Some(token) = token_for_host(host) {
            request.header(GITLAB_TOKEN_HEADER, token)
        } else {
            request
        }
    }

    fn with_github_auth<B>(request: RequestBuilder<B>, token: &str) -> RequestBuilder<B> {
        request.header(AUTHORIZATION, Self::bearer(token))
    }

    fn bearer(token: &str) -> String {
        format!("Bearer {token}")
    }

    pub(super) fn github_json<T>(self, url: &str, timeout_limit: Option<Duration>) -> FetchResult<T>
    where
        T: DeserializeOwned,
    {
        let mut req = self.github_get(url);
        if let Some(timeout) = timeout_limit {
            req = req.config().timeout_global(Some(timeout)).build();
        }

        let mut resp = req.call().map_err(|err| FetchError::from_ureq(err, url))?;
        let status = resp.status();
        if status != 200 {
            return Err(FetchError::from_status(status.as_u16(), url));
        }

        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|err| FetchError::Transport(format!("read github api {url}: {err}")))?;

        serde_json::from_str::<T>(&body)
            .map_err(|err| FetchError::Github(format!("api {url}: invalid json: {err}")))
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
        let mut req =
            Self::with_optional_gitlab_auth(self.get(url).header(ACCEPT, APPLICATION_JSON), host);
        if let Some(timeout) = timeout_limit {
            req = req.config().timeout_global(Some(timeout)).build();
        }

        let mut resp = req.call().map_err(|err| FetchError::from_ureq(err, url))?;
        let status = resp.status();
        if status != 200 {
            return Err(FetchError::from_status(status.as_u16(), url));
        }

        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|err| FetchError::Transport(format!("read gitlab api {url}: {err}")))?;

        serde_json::from_str::<T>(&body)
            .map_err(|err| FetchError::Gitlab(format!("api {url}: invalid json: {err}")))
    }

    pub(super) fn github_graphql<V, T>(self, query: &str, variables: &V) -> FetchResult<T>
    where
        V: Serialize,
        T: DeserializeOwned,
    {
        let token = token_for_host("github.com").ok_or_else(|| {
            record_token_warning(
                "no GitHub token; rev comparison falls back to commit lookup plus git DAG compare \
                 (set GITHUB_TOKEN or GH_TOKEN, or opt into nix.conf access-tokens with \
                 TACK_NIX_CONF_TOKENS=1)"
                    .to_owned(),
            );
            FetchError::Auth {
                what: "GITHUB_TOKEN or GH_TOKEN not set".to_owned(),
            }
        })?;

        let payload = serde_json::to_string(&GraphqlRequest { query, variables })
            .map_err(|err| FetchError::Github(format!("serialize graphql request: {err}")))?;

        let mut resp = self
            .github_post(GITHUB_GRAPHQL_URL, token)
            .config()
            .timeout_global(Some(GITHUB_GRAPHQL_TIMEOUT))
            .build()
            .send(payload)
            .map_err(|err| FetchError::from_ureq(err, "github graphql"))?;

        let status = resp.status();
        if status != 200 {
            return Err(FetchError::from_status(status.as_u16(), "github graphql"));
        }

        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|err| FetchError::Transport(format!("read github graphql: {err}")))?;

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
    }

    pub(super) fn raw_text(self, url: &str) -> FetchResult<String> {
        let mut resp = self
            .get(url)
            .call()
            .map_err(|err| FetchError::from_ureq(err, url))?;
        let status = resp.status();
        if status != 200 {
            return Err(FetchError::from_status(status.as_u16(), url));
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

    use super::{
        FetchError,
        env_flag_enabled,
        scrape_access_tokens,
    };

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
