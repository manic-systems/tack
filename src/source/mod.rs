// SPDX-License-Identifier: EUPL-1.2

pub mod forge;
pub mod id;

pub mod gitlab;
mod host;

use std::{
    borrow::Cow,
    str::FromStr,
};

use eyre::{
    Result,
    bail,
};

/// fetchable pin source, from an expanded pins.toml url
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
    Gitlab {
        host:  String,
        owner: String,
        repo:  String,
        reff:  Option<String>,
        rev:   Option<String>,
    },
    Tarball {
        url: String,
    },
}

pub struct GitTarget<'a> {
    pub url:  Cow<'a, str>,
    pub reff: Option<&'a str>,
    pub rev:  Option<&'a str>,
}

impl Source {
    pub fn git_target(&self) -> Option<GitTarget<'_>> {
        match *self {
            Self::Git {
                ref url,
                ref reff,
                ref rev,
            } => {
                Some(GitTarget {
                    url:  Cow::Borrowed(url),
                    reff: reff.as_deref(),
                    rev:  rev.as_deref(),
                })
            },
            Self::Gitlab {
                ref host,
                ref owner,
                ref repo,
                ref reff,
                ref rev,
            } => {
                Some(GitTarget {
                    url:  Cow::Owned(gitlab::clone_url(host, owner, repo)),
                    reff: reff.as_deref(),
                    rev:  rev.as_deref(),
                })
            },
            Self::Github { .. } | Self::Tarball { .. } => None,
        }
    }
}

impl FromStr for Source {
    type Err = eyre::Report;

    fn from_str(expanded: &str) -> Result<Self> {
        if let Some(body) = expanded.strip_prefix("github:") {
            let (path, raw_query) = split_query_fragment(body);
            let fields = parse_query_fields(raw_query);
            let segs = path.split('/').collect::<Vec<&str>>();
            if segs.len() < 2 {
                bail!("malformed github url: {expanded}");
            }
            let reff = fields
                .reff
                .map(ToOwned::to_owned)
                .or_else(|| (segs.len() > 2).then(|| segs[2..].join("/")));
            return Ok(Self::Github {
                owner: segs[0].to_owned(),
                repo: segs[1].to_owned(),
                reff,
                rev: fields.rev.map(ToOwned::to_owned),
            });
        }
        if let Some(body) = expanded.strip_prefix("gitlab:") {
            let Some(parsed) = gitlab::parse_flake_url(body) else {
                bail!("malformed gitlab url: {expanded}");
            };
            return Ok(Self::Gitlab {
                host:  parsed.host,
                owner: parsed.owner,
                repo:  parsed.repo,
                reff:  parsed.reff,
                rev:   parsed.rev,
            });
        }
        if let Some(rest) = expanded.strip_prefix("git+") {
            let (url, raw_query) = split_query_fragment(rest);
            let fields = parse_query_fields(raw_query);
            return Ok(Self::Git {
                url:  url.to_owned(),
                reff: fields.reff.map(ToOwned::to_owned),
                rev:  fields.rev.map(ToOwned::to_owned),
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

fn split_query_fragment(value: &str) -> (&str, Option<&str>) {
    let without_fragment = strip_fragment(value);
    without_fragment
        .split_once('?')
        .map_or((without_fragment, None), |(path, query)| {
            (path, Some(query))
        })
}

fn strip_query_fragment(value: &str) -> &str {
    split_query_fragment(value).0
}

fn strip_fragment(value: &str) -> &str {
    value
        .split_once('#')
        .map_or(value, |(without_fragment, _fragment)| without_fragment)
}

#[derive(Default)]
struct QueryFields<'a> {
    pub host: Option<&'a str>,
    pub reff: Option<&'a str>,
    pub rev:  Option<&'a str>,
}

fn parse_query_fields(query: Option<&str>) -> QueryFields<'_> {
    let mut fields = QueryFields::default();
    let Some(raw_query) = query else {
        return fields;
    };
    for kv in raw_query.split('&') {
        let Some((key, value)) = kv.split_once('=') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        match key {
            "host" => fields.host = Some(value),
            "ref" => fields.reff = Some(value),
            "rev" => fields.rev = Some(value),
            _ => {},
        }
    }
    fields
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
            Source::Github { .. } | Source::Gitlab { .. } | Source::Tarball { .. } => {
                panic!("expected git target")
            },
        }
        match "git+ssh://git@example.com/o/r?rev=deadbeef"
            .parse::<Source>()
            .unwrap()
        {
            Source::Git { reff, rev, .. } => {
                assert_eq!(reff, None);
                assert_eq!(rev.as_deref(), Some("deadbeef"));
            },
            Source::Github { .. } | Source::Gitlab { .. } | Source::Tarball { .. } => {
                panic!("expected git target")
            },
        }
    }

    #[test]
    fn git_query_fragment_is_not_part_of_ref_or_rev() {
        match "git+https://example.com/o/r?ref=main&rev=abc123#frag"
            .parse::<Source>()
            .unwrap()
        {
            Source::Git { url, reff, rev } => {
                assert_eq!(url, "https://example.com/o/r");
                assert_eq!(reff.as_deref(), Some("main"));
                assert_eq!(rev.as_deref(), Some("abc123"));
            },
            Source::Github { .. } | Source::Gitlab { .. } | Source::Tarball { .. } => {
                panic!("expected git target")
            },
        }
    }

    #[test]
    fn gitlab_url_is_first_class_source() {
        match "gitlab:NixOS/nixpkgs/nixos-unstable?host=GitLab.Example.Com&rev=abc123"
            .parse::<Source>()
            .unwrap()
        {
            Source::Gitlab {
                host,
                owner,
                repo,
                reff,
                rev,
            } => {
                assert_eq!(host, "gitlab.example.com");
                assert_eq!(owner, "NixOS");
                assert_eq!(repo, "nixpkgs");
                assert_eq!(reff.as_deref(), Some("nixos-unstable"));
                assert_eq!(rev.as_deref(), Some("abc123"));
            },
            Source::Github { .. } | Source::Git { .. } | Source::Tarball { .. } => {
                panic!("expected gitlab target")
            },
        }
    }

    #[test]
    fn git_target_covers_git_and_gitlab_sources() {
        let git = "git+https://example.com/o/r?ref=main&rev=abc123"
            .parse::<Source>()
            .unwrap();
        let git_target = git.git_target().unwrap();
        assert_eq!(git_target.url.as_ref(), "https://example.com/o/r");
        assert_eq!(git_target.reff, Some("main"));
        assert_eq!(git_target.rev, Some("abc123"));

        let gitlab = "gitlab:NixOS/nixpkgs/nixos-unstable?host=gitlab.example.com&rev=abc123"
            .parse::<Source>()
            .unwrap();
        let gitlab_target = gitlab.git_target().unwrap();
        assert_eq!(
            gitlab_target.url.as_ref(),
            "https://gitlab.example.com/NixOS/nixpkgs.git"
        );
        assert_eq!(gitlab_target.reff, Some("nixos-unstable"));
        assert_eq!(gitlab_target.rev, Some("abc123"));
    }

    #[test]
    fn github_rev_is_committish() {
        match "github:o/r?rev=abc123".parse::<Source>().unwrap() {
            Source::Github { reff, rev, .. } => {
                assert_eq!(reff, None);
                assert_eq!(rev.as_deref(), Some("abc123"));
            },
            Source::Git { .. } | Source::Gitlab { .. } | Source::Tarball { .. } => {
                panic!("expected github target")
            },
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
            Source::Github { .. } | Source::Git { .. } | Source::Gitlab { .. } => {
                panic!("expected tarball target")
            },
        }
        match "http://example.com/release.tar.gz"
            .parse::<Source>()
            .unwrap()
        {
            Source::Tarball { .. } => {},
            Source::Github { .. } | Source::Git { .. } | Source::Gitlab { .. } => {
                panic!("expected tarball target")
            },
        }
    }
}
