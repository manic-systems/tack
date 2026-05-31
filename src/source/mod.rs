// SPDX-License-Identifier: EUPL-1.2

pub mod forge;
pub mod id;

use std::str::FromStr;

use anyhow::{
    Result,
    bail,
};

/// A fetchable pin source, parsed from an expanded pins.toml URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Github {
        owner: String,
        repo:  String,
        reff:  Option<String>,
        rev:   Option<String>,
    },
    Git {
        url:  String,
        reff: Option<String>,
        rev:  Option<String>,
    },
    Tarball {
        url: String,
    },
}

impl FromStr for Source {
    type Err = anyhow::Error;

    #[expect(
        clippy::similar_names,
        reason = "ref and rev are user-facing URL fields"
    )]
    fn from_str(expanded: &str) -> Result<Self> {
        if let Some(body) = expanded.strip_prefix("github:") {
            let (path, query_ref, query_rev) = split_query(body);
            let segs = path.split('/').collect::<Vec<&str>>();
            if segs.len() < 2 {
                bail!("malformed github url: {expanded}");
            }
            let reff = query_ref.or_else(|| (segs.len() > 2).then(|| segs[2..].join("/")));
            return Ok(Self::Github {
                owner: segs[0].to_owned(),
                repo: segs[1].to_owned(),
                reff,
                rev: query_rev,
            });
        }
        if let Some(rest) = expanded.strip_prefix("git+") {
            let (url, reff, rev) = split_query(rest);
            return Ok(Self::Git {
                url: url.to_owned(),
                reff,
                rev,
            });
        }
        if expanded.starts_with("https://") || expanded.starts_with("http://") {
            return Ok(Self::Tarball {
                url: expanded.to_owned(),
            });
        }
        bail!("unsupported url scheme: {expanded}")
    }
}

/// Pull out ref= and rev=.
fn split_query(str: &str) -> (&str, Option<String>, Option<String>) {
    let Some((path, query)) = str.split_once('?') else {
        return (str, None, None);
    };
    let (mut reff, mut rev) = (None, None);
    for kv in query.split('&') {
        if let Some(value) = kv.strip_prefix("ref=") {
            reff = Some(value.to_owned());
        } else if let Some(value) = kv.strip_prefix("rev=") {
            rev = Some(value.to_owned());
        }
    }
    (path, reff, rev)
}

#[cfg(test)]
#[expect(clippy::panic, reason = "panic is the test-failure coping mechanism")]
mod tests {
    use super::Source;

    #[test]
    fn git_rev_query() {
        match "git+https://example.com/o/r?ref=main&rev=abc123"
            .parse::<Source>()
            .unwrap()
        {
            Source::Git { url, reff, rev } => {
                assert_eq!(url, "https://example.com/o/r");
                assert_eq!(reff.as_deref(), Some("main"));
                assert_eq!(rev.as_deref(), Some("abc123"));
            },
            Source::Github { .. } | Source::Tarball { .. } => panic!("expected git target"),
        }
        match "git+ssh://git@example.com/o/r?rev=deadbeef"
            .parse::<Source>()
            .unwrap()
        {
            Source::Git { reff, rev, .. } => {
                assert_eq!(reff, None);
                assert_eq!(rev.as_deref(), Some("deadbeef"));
            },
            Source::Github { .. } | Source::Tarball { .. } => panic!("expected git target"),
        }
    }

    #[test]
    fn github_rev_is_committish() {
        match "github:o/r?rev=abc123".parse::<Source>().unwrap() {
            Source::Github { reff, rev, .. } => {
                assert_eq!(reff, None);
                assert_eq!(rev.as_deref(), Some("abc123"));
            },
            Source::Git { .. } | Source::Tarball { .. } => panic!("expected github target"),
        }
    }

    #[test]
    fn https_url_is_tarball() {
        match "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz"
            .parse::<Source>()
            .unwrap()
        {
            Source::Tarball { url } => {
                assert_eq!(
                    url,
                    "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz"
                );
            },
            Source::Github { .. } | Source::Git { .. } => panic!("expected tarball target"),
        }
        match "http://example.com/release.tar.gz"
            .parse::<Source>()
            .unwrap()
        {
            Source::Tarball { .. } => {},
            Source::Github { .. } | Source::Git { .. } => panic!("expected tarball target"),
        }
    }
}
