// SPDX-License-Identifier: EUPL-1.2

use std::{
    env,
    error::Error,
    io::Read as _,
    result::Result as StdResult,
    sync::OnceLock,
    time::Duration,
};

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
    tls::{
        TlsConfig,
        TlsProvider,
    },
    typestate::{
        WithBody,
        WithoutBody,
    },
};

const TACK_USER_AGENT: &str = "tack";
const GITHUB_ACCEPT: &str = "application/vnd.github+json";
const GITHUB_GRAPHQL_URL: &str = "https://api.github.com/graphql";
const APPLICATION_JSON: &str = "application/json";
const GITLAB_TOKEN_HEADER: &str = "PRIVATE-TOKEN";

fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let config = TlsConfig::builder()
            .provider(TlsProvider::NativeTls)
            .build();
        Agent::config_builder().tls_config(config).build().into()
    })
}

fn github_token() -> Option<&'static str> {
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            env::var("GITHUB_TOKEN")
                .or_else(|_| env::var("GH_TOKEN"))
                .ok()
        })
        .as_deref()
}

fn gitlab_token() -> Option<&'static str> {
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN
        .get_or_init(|| env::var("GITLAB_TOKEN").ok())
        .as_deref()
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
}

impl FetchError {
    fn from_status(status: u16, what: &str) -> Self {
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
    fn from_ureq(err: ureq::Error, what: &str) -> Self {
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
        if let Some(token) = github_token() {
            Self::with_github_auth(request, token)
        } else {
            request
        }
    }

    fn with_optional_gitlab_auth<B>(request: RequestBuilder<B>) -> RequestBuilder<B> {
        if let Some(token) = gitlab_token() {
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

    pub(super) fn gitlab_json<T>(self, url: &str, timeout_limit: Option<Duration>) -> FetchResult<T>
    where
        T: DeserializeOwned,
    {
        let mut req =
            Self::with_optional_gitlab_auth(self.get(url).header(ACCEPT, APPLICATION_JSON));
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
        let token = github_token().ok_or_else(|| {
            FetchError::Auth {
                what: "GITHUB_TOKEN or GH_TOKEN not set".to_owned(),
            }
        })?;

        let payload = serde_json::to_string(&GraphqlRequest { query, variables })
            .map_err(|err| FetchError::Github(format!("serialize graphql request: {err}")))?;

        let mut resp = self
            .github_post(GITHUB_GRAPHQL_URL, token)
            .config()
            .timeout_global(Some(Duration::from_secs(2)))
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
    use super::FetchError;

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
