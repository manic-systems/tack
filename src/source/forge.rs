// SPDX-License-Identifier: EUPL-1.2

use std::error::Error;

use crate::lock;

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
    pub fn from_locked(node: &lock::LockedNode) -> Option<Self> {
        let (base, authoritative) = match node.kind() {
            "github" => {
                let github = node.github()?;
                (
                    format!(
                        "https://raw.githubusercontent.com/{}/{}",
                        github.owner, github.repo
                    ),
                    true,
                )
            },
            "gitlab" => {
                let gitlab = node.gitlab()?;
                (
                    format!("https://{}/{}/{}", gitlab.host, gitlab.owner, gitlab.repo),
                    true,
                )
            },
            "git" => {
                let url = node.git()?.url;
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
    use crate::lock;

    fn node(value: serde_json::Value) -> lock::LockedNode {
        lock::LockedNode::from_value(value).unwrap()
    }

    fn url(node: lock::LockedNode, file: &str) -> Option<String> {
        Forge::from_locked(&node).map(|forge| forge.raw_file_url("REV", file).url)
    }

    #[test]
    fn github_node_builds_raw_githubusercontent_url() {
        assert_eq!(
            url(
                node(json!({"type": "github", "owner": "o", "repo": "r"})),
                "flake.lock"
            )
            .as_deref(),
            Some("https://raw.githubusercontent.com/o/r/REV/flake.lock")
        );
    }

    #[test]
    fn gitlab_node_uses_dash_raw_and_is_authoritative() {
        let forge = Forge::from_locked(&node(json!({"type": "gitlab", "owner": "o", "repo": "r"})))
            .unwrap();
        assert_eq!(
            forge.raw_file_url("REV", "f").url,
            "https://gitlab.com/o/r/-/raw/REV/f"
        );
        assert!(forge.authoritative());
    }

    #[test]
    fn git_node_is_not_authoritative_and_uses_gitea_default() {
        let forge = Forge::from_locked(&node(
            json!({"type": "git", "url": "https://codeberg.org/o/r.git"}),
        ))
        .unwrap();
        assert!(!forge.authoritative());
        assert_eq!(
            forge.raw_file_url("REV", "f").url,
            "https://codeberg.org/o/r/raw/commit/REV/f"
        );
    }

    #[test]
    fn gerrit_host_decodes_base64() {
        let forge = Forge::from_locked(&node(
            json!({"type": "git", "url": "https://x.googlesource.com/o/r"}),
        ))
        .unwrap();
        let raw = forge.raw_file_url("REV", "f");
        assert_eq!(
            raw.url,
            "https://x.googlesource.com/o/r/+/REV/f?format=TEXT"
        );
        assert!(raw.decoder.is_some());
    }
}
