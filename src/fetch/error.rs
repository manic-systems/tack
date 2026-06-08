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

#[cfg(test)]
mod tests {
    use ureq::http::{
        HeaderMap,
        HeaderName,
    };

    use super::{
        ApiErrorBody,
        FetchError,
        rate_limit_hint,
    };

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

    // ureq surfaces non-2xx as Error::StatusCode
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
