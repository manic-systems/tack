// SPDX-License-Identifier: EUPL-1.2

use std::{
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
    Body,
    RequestBuilder,
    http::{
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

use super::{
    auth::{
        Credential,
        record_fetch_warning,
        token_for_host,
        with_credential_fallback,
    },
    error::{
        FetchError,
        FetchResult,
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

fn read_ok_body(resp: &mut Response<Body>, what: &str) -> FetchResult<String> {
    if !resp.status().is_success() {
        return Err(FetchError::from_response(resp, what));
    }
    resp.body_mut()
        .read_to_string()
        .map_err(|err| FetchError::Transport(format!("read {what}: {err}")))
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

    pub(super) fn with_gitlab_credential<B>(
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
            record_fetch_warning(format!(
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
            record_fetch_warning(
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
