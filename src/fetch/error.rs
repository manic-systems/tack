// SPDX-License-Identifier: EUPL-1.2

use std::{
    error::Error,
    io::Read as _,
    time::{
        SystemTime,
        UNIX_EPOCH,
    },
};

use serde::Deserialize;
use ureq::{
    Body,
    http::{
        HeaderMap,
        Response,
    },
};

pub type FetchResult<T> = Result<T, FetchError>;

#[derive(thiserror::Error, Debug)]
pub enum FetchError {
    #[error("{what} not found")]
    NotFound { what: String },

    #[error("auth: {what}")]
    Auth { what: String },

    #[error("rate limited: {what}")]
    RateLimited { what: String },

    #[error("network: {0}")]
    Transport(String),

    #[error("decoding {what}")]
    Decode {
        what:   String,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },

    #[error("unexpected github response: {0}")]
    Github(String),

    #[error("unexpected gitlab response: {0}")]
    Gitlab(String),

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

    pub(super) const fn is_credential_failure(&self) -> bool {
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
