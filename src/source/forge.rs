// SPDX-License-Identifier: EUPL-1.2

use std::error::Error;

use crate::lock::LockedNode;

/// body decoder applied after a raw-file http get
pub type DecoderError = Box<dyn Error + Send + Sync>;
pub type Decoder = fn(&str) -> Result<String, DecoderError>;

/// resolved raw-file request
pub struct RawFile {
    pub url:     String,
    pub decoder: Option<Decoder>,
}

struct HostScheme {
    matches: fn(host: &str) -> bool,
    build:   fn(base: &str, rev: &str, file: &str) -> String,
    decoder: Option<Decoder>,
}

static GITHUB_RAW_SCHEME: HostScheme = HostScheme {
    matches: |host| host == "raw.githubusercontent.com",
    build:   |base, rev, file| format!("{base}/{rev}/{file}"),
    decoder: None,
};

static GITLAB_SCHEME: HostScheme = HostScheme {
    matches: |host| host == "gitlab.com" || host.starts_with("gitlab."),
    build:   |base, rev, file| format!("{base}/-/raw/{rev}/{file}"),
    decoder: None,
};

static BITBUCKET_SCHEME: HostScheme = HostScheme {
    matches: |host| host == "bitbucket.org",
    build:   |base, rev, file| format!("{base}/raw/{rev}/{file}"),
    decoder: None,
};

static CGIT_SCHEME: HostScheme = HostScheme {
    matches: |host| host.starts_with("cgit.") || host == "git.kernel.org",
    build:   |base, rev, file| format!("{base}/plain/{file}?id={rev}"),
    decoder: None,
};

static GERRIT_SCHEME: HostScheme = HostScheme {
    matches: |host| host.ends_with(".googlesource.com") || host.starts_with("gerrit."),
    build:   |base, rev, file| format!("{base}/+/{rev}/{file}?format=TEXT"),
    decoder: Some(decode_b64),
};

static GIT_SCHEMES: &[&HostScheme] = &[
    &GITLAB_SCHEME,
    &BITBUCKET_SCHEME,
    &CGIT_SCHEME,
    &GERRIT_SCHEME,
];

static DEFAULT_SCHEME: HostScheme = HostScheme {
    matches: |_| true,
    build:   |base, rev, file| format!("{base}/raw/commit/{rev}/{file}"),
    decoder: None,
};

/// repo whose raw files can be probed over http
pub struct Forge {
    base:          String,
    authoritative: bool,
    scheme:        &'static HostScheme,
}

impl Forge {
    pub fn from_locked(node: &LockedNode) -> Option<Self> {
        let (base, authoritative, scheme) = match *node {
            LockedNode::Github {
                ref owner,
                ref repo,
                ..
            } => {
                (
                    format!("https://raw.githubusercontent.com/{owner}/{repo}"),
                    true,
                    &GITHUB_RAW_SCHEME,
                )
            },
            LockedNode::Gitlab {
                ref host,
                ref owner,
                ref repo,
                ..
            } => {
                (
                    format!("https://{host}/{owner}/{repo}"),
                    true,
                    &GITLAB_SCHEME,
                )
            },
            LockedNode::Git { ref url, .. } => {
                let base = url.strip_suffix(".git").unwrap_or(url).to_owned();
                let scheme = scheme_for_git_url(&base);
                (base, false, scheme)
            },
            LockedNode::Tarball { .. }
            | LockedNode::Fixed { .. }
            | LockedNode::Indirect { .. }
            | LockedNode::Path { .. } => return None,
        };
        Some(Self {
            base,
            authoritative,
            scheme,
        })
    }

    pub const fn authoritative(&self) -> bool {
        self.authoritative
    }

    pub fn raw_file_url(&self, rev: &str, file: &str) -> RawFile {
        RawFile {
            url:     (self.scheme.build)(&self.base, rev, file),
            decoder: self.scheme.decoder,
        }
    }
}

fn scheme_for_git_url(base: &str) -> &'static HostScheme {
    let host = host_of(base);
    GIT_SCHEMES
        .iter()
        .copied()
        .find(|scheme| (scheme.matches)(host))
        .unwrap_or(&DEFAULT_SCHEME)
}

fn host_of(base: &str) -> &str {
    base.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("")
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

    use super::{
        Forge,
        LockedNode,
    };

    fn node(value: serde_json::Value) -> LockedNode {
        LockedNode::from_value(value).unwrap()
    }

    fn url(node: &LockedNode, file: &str) -> Option<String> {
        Forge::from_locked(node).map(|forge| forge.raw_file_url("REV", file).url)
    }

    #[test]
    fn github_node_builds_raw_githubusercontent_url() {
        assert_eq!(
            url(
                &node(json!({"type": "github", "owner": "o", "repo": "r"})),
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
    fn self_hosted_gitlab_node_keeps_gitlab_raw_scheme() {
        let forge = Forge::from_locked(&node(
            json!({"type": "gitlab", "host": "git.example.com", "owner": "o", "repo": "r"}),
        ))
        .unwrap();
        assert_eq!(
            forge.raw_file_url("REV", "f").url,
            "https://git.example.com/o/r/-/raw/REV/f"
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
