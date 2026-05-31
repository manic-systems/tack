// SPDX-License-Identifier: EUPL-1.2

use std::error::Error;

use serde_json::Value;

/// Body decoder applied after a raw-file HTTP get.
pub type DecoderError = Box<dyn Error + Send + Sync>;
pub type Decoder = fn(&str) -> Result<String, DecoderError>;

/// A resolved raw-file request.
pub struct RawFile {
    pub url:     String,
    pub decoder: Option<Decoder>,
}

struct HostScheme {
    matches: fn(host: &str) -> bool,
    build:   fn(base: &str, rev: &str, file: &str) -> String,
    decoder: Option<Decoder>,
}

const SCHEMES: &[HostScheme] = &[
    HostScheme {
        matches: |host| host == "raw.githubusercontent.com",
        build:   |base, rev, file| format!("{base}/{rev}/{file}"),
        decoder: None,
    },
    HostScheme {
        matches: |host| host == "gitlab.com" || host.starts_with("gitlab."),
        build:   |base, rev, file| format!("{base}/-/raw/{rev}/{file}"),
        decoder: None,
    },
    HostScheme {
        matches: |host| host == "bitbucket.org",
        build:   |base, rev, file| format!("{base}/raw/{rev}/{file}"),
        decoder: None,
    },
    HostScheme {
        matches: |host| host.starts_with("cgit.") || host == "git.kernel.org",
        build:   |base, rev, file| format!("{base}/plain/{file}?id={rev}"),
        decoder: None,
    },
    HostScheme {
        matches: |host| host.ends_with(".googlesource.com") || host.starts_with("gerrit."),
        build:   |base, rev, file| format!("{base}/+/{rev}/{file}?format=TEXT"),
        decoder: Some(decode_b64),
    },
];

const DEFAULT_SCHEME: HostScheme = HostScheme {
    matches: |_| true,
    build:   |base, rev, file| format!("{base}/raw/commit/{rev}/{file}"),
    decoder: None,
};

/// A repo whose raw files can be probed over HTTP.
pub struct Forge {
    base:          String,
    authoritative: bool,
}

impl Forge {
    pub fn from_node(node: &Value) -> Option<Self> {
        let ty = node.get("type").and_then(Value::as_str)?;
        let owner_repo = || -> Option<(&str, &str)> {
            Some((node.get("owner")?.as_str()?, node.get("repo")?.as_str()?))
        };
        let (base, authoritative) = match ty {
            "github" => {
                let (owner, repo) = owner_repo()?;
                (
                    format!("https://raw.githubusercontent.com/{owner}/{repo}"),
                    true,
                )
            },
            "gitlab" => {
                let (owner, repo) = owner_repo()?;
                let host = node
                    .get("host")
                    .and_then(Value::as_str)
                    .unwrap_or("gitlab.com");
                (format!("https://{host}/{owner}/{repo}"), true)
            },
            "git" => {
                let url = node.get("url").and_then(Value::as_str)?;
                (url.strip_suffix(".git").unwrap_or(url).to_owned(), false)
            },
            _ => return None,
        };
        Some(Self {
            base,
            authoritative,
        })
    }

    pub const fn authoritative(&self) -> bool {
        self.authoritative
    }

    pub fn raw_file_url(&self, rev: &str, file: &str) -> RawFile {
        let host = self
            .base
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or("");
        let scheme = SCHEMES
            .iter()
            .find(|scheme| (scheme.matches)(host))
            .unwrap_or(&DEFAULT_SCHEME);
        RawFile {
            url:     (scheme.build)(&self.base, rev, file),
            decoder: scheme.decoder,
        }
    }
}

fn decode_b64(body: &str) -> Result<String, DecoderError> {
    let bytes = data_encoding::BASE64
        .decode(body.trim().as_bytes())
        .map_err(|err| Box::new(err) as DecoderError)?;
    String::from_utf8(bytes).map_err(|err| Box::new(err) as DecoderError)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Forge;

    fn url(node: &serde_json::Value, file: &str) -> Option<String> {
        Forge::from_node(node).map(|forge| forge.raw_file_url("REV", file).url)
    }

    #[test]
    fn github_node_builds_raw_githubusercontent_url() {
        assert_eq!(
            url(
                &json!({"type": "github", "owner": "o", "repo": "r"}),
                "flake.lock"
            )
            .as_deref(),
            Some("https://raw.githubusercontent.com/o/r/REV/flake.lock")
        );
    }

    #[test]
    fn gitlab_node_uses_dash_raw_and_is_authoritative() {
        let forge =
            Forge::from_node(&json!({"type": "gitlab", "owner": "o", "repo": "r"})).unwrap();
        assert_eq!(
            forge.raw_file_url("REV", "f").url,
            "https://gitlab.com/o/r/-/raw/REV/f"
        );
        assert!(forge.authoritative());
    }

    #[test]
    fn git_node_is_not_authoritative_and_uses_gitea_default() {
        let forge =
            Forge::from_node(&json!({"type": "git", "url": "https://codeberg.org/o/r.git"}))
                .unwrap();
        assert!(!forge.authoritative());
        assert_eq!(
            forge.raw_file_url("REV", "f").url,
            "https://codeberg.org/o/r/raw/commit/REV/f"
        );
    }

    #[test]
    fn gerrit_host_decodes_base64() {
        let forge =
            Forge::from_node(&json!({"type": "git", "url": "https://x.googlesource.com/o/r"}))
                .unwrap();
        let raw = forge.raw_file_url("REV", "f");
        assert_eq!(
            raw.url,
            "https://x.googlesource.com/o/r/+/REV/f?format=TEXT"
        );
        assert!(raw.decoder.is_some());
    }
}
