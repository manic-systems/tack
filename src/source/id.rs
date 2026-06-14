// SPDX-License-Identifier: EUPL-1.2

use std::{
    cmp::Ordering,
    fmt::{
        self,
        Display,
    },
};

use crate::{
    lock::LockedNode,
    source::{
        Source,
        git_url,
        gitlab,
        host,
        strip_query_fragment,
    },
};

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum SourceId {
    Github {
        owner: String,
        repo:  String,
    },
    Gitlab {
        host:  String,
        owner: String,
        repo:  String,
    },
    Git {
        url: String,
    },
    Tarball {
        url: String,
    },
    Indirect {
        id: String,
    },
    Path {
        path: String,
    },
}

/// github.com has no subgroups
fn github_repo_from_git_url(url: &str) -> Option<(String, String)> {
    let repo = git_url::parse(url)?;
    (repo.host == "github.com" && !repo.owner.contains('/')).then_some((repo.owner, repo.repo))
}

impl SourceId {
    pub fn from_url(expanded: &str) -> Option<Self> {
        let source = expanded.parse::<Source>().ok()?;
        Some(Self::from_source(&source))
    }

    fn from_source(source: &Source) -> Self {
        match *source {
            Source::Github {
                ref owner,
                ref repo,
                ..
            } => Self::github(owner, repo),
            Source::Gitlab {
                ref host,
                ref owner,
                ref repo,
                ..
            } => Self::gitlab(host, owner, repo),
            Source::Git { ref url, .. } => {
                if let Some(repo) = gitlab::parse_git_url(url) {
                    return Self::gitlab(&repo.host, &repo.owner, &repo.repo);
                }
                if let Some((owner, repo)) = github_repo_from_git_url(url) {
                    return Self::github(&owner, &repo);
                }
                Self::Git {
                    url: url.to_lowercase(),
                }
            },
            Source::Tarball { ref url } => {
                Self::Tarball {
                    url: strip_query_fragment(url).to_lowercase(),
                }
            },
            Source::Path { ref path } => {
                Self::Path {
                    path: path.to_lowercase(),
                }
            },
        }
    }

    pub fn from_locked(node: &LockedNode) -> Option<Self> {
        match *node {
            LockedNode::Github {
                ref owner,
                ref repo,
                ..
            } => Some(Self::github(owner, repo)),
            LockedNode::Gitlab {
                ref host,
                ref owner,
                ref repo,
                ..
            } => Some(Self::gitlab(host, owner, repo)),
            LockedNode::Git { ref url, .. } => {
                let cut = strip_query_fragment(url);
                if let Some(repo) = gitlab::parse_git_url(cut) {
                    return Some(Self::gitlab(&repo.host, &repo.owner, &repo.repo));
                }
                if let Some((owner, repo)) = github_repo_from_git_url(cut) {
                    return Some(Self::github(&owner, &repo));
                }
                Some(Self::Git {
                    url: cut.to_lowercase(),
                })
            },
            LockedNode::Tarball { ref url, .. } => {
                Some(Self::Tarball {
                    url: strip_query_fragment(url).to_lowercase(),
                })
            },
            LockedNode::Indirect { ref id, .. } => {
                Some(Self::Indirect {
                    id: id.to_lowercase(),
                })
            },
            LockedNode::Path { ref path, .. } => {
                Some(Self::Path {
                    path: path.to_lowercase(),
                })
            },
            LockedNode::Fixed { .. } => None,
        }
    }

    fn github(owner: &str, repo: &str) -> Self {
        Self::Github {
            owner: owner.to_lowercase(),
            repo:  repo.to_lowercase(),
        }
    }

    fn gitlab(host: &str, owner: &str, repo: &str) -> Self {
        Self::Gitlab {
            host:  host::normalized(host),
            owner: owner.to_lowercase(),
            repo:  repo.to_lowercase(),
        }
    }

    pub fn repo_name(&self) -> Option<&str> {
        match *self {
            Self::Github { ref repo, .. } | Self::Gitlab { ref repo, .. } => Some(repo),
            Self::Git { .. } | Self::Tarball { .. } | Self::Indirect { .. } | Self::Path { .. } => {
                None
            },
        }
    }
}

impl Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Github {
                ref owner,
                ref repo,
            } => write!(f, "github:{owner}/{repo}"),
            Self::Gitlab {
                ref host,
                ref owner,
                ref repo,
            } => write!(f, "gitlab:{host}/{owner}/{repo}"),
            Self::Git { ref url } => write!(f, "git+{url}"),
            Self::Tarball { ref url } => write!(f, "tarball:{url}"),
            Self::Indirect { ref id } => write!(f, "indirect:{id}"),
            Self::Path { ref path } => write!(f, "path:{path}"),
        }
    }
}

impl Ord for SourceId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.to_string().cmp(&other.to_string())
    }
}

impl PartialOrd for SourceId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        LockedNode,
        SourceId,
    };

    fn node(value: serde_json::Value) -> LockedNode {
        LockedNode::from_value(value).unwrap()
    }

    #[test]
    fn source_identity_normalizes_common_url_and_lock_forms() {
        let cases: [(Option<&str>, Option<serde_json::Value>, &str); 5] = [
            (
                Some("github:NixOS/nixpkgs/nixos-unstable"),
                Some(json!({"type": "github", "owner": "NixOS", "repo": "Nixpkgs"})),
                "github:nixos/nixpkgs",
            ),
            (
                Some("gitlab:NixOS/nixpkgs?host=Git.Example.Com"),
                Some(
                    json!({"type": "gitlab", "host": "git.example.com", "owner": "NixOS", "repo": "nixpkgs"}),
                ),
                "gitlab:git.example.com/nixos/nixpkgs",
            ),
            (
                Some("git+https://github.com/o/r.git?ref=main"),
                None,
                "github:o/r",
            ),
            (
                Some("git+https://x.com/o/r?ref=main#frag"),
                Some(json!({"type": "git", "url": "https://x.com/o/r?ref=main#frag"})),
                "git+https://x.com/o/r",
            ),
            (
                Some("path:/P/X"),
                Some(json!({"type": "path", "path": "/p/x"})),
                "path:/p/x",
            ),
        ];

        for (url_case, locked_case, expected) in cases {
            if let Some(source_url) = url_case {
                assert_eq!(
                    SourceId::from_url(source_url).unwrap().to_string(),
                    expected
                );
            }
            if let Some(locked_value) = locked_case {
                assert_eq!(
                    SourceId::from_locked(&node(locked_value))
                        .unwrap()
                        .to_string(),
                    expected
                );
            }
        }
    }

    #[test]
    fn gitlab_identity_keeps_nested_groups_and_self_hosted_boundaries() {
        let nested = SourceId::from_url("gitlab:group%2Fsub/repo").unwrap();
        let ssh = SourceId::from_url("git+ssh://git@gitlab.com:2222/group/sub/repo.git").unwrap();
        let self_hosted = SourceId::from_locked(&node(
            json!({"type": "gitlab", "host": "Git.Example.Com", "owner": "group/sub", "repo": "repo"}),
        ))
        .unwrap();

        assert_eq!(nested, ssh);
        assert_ne!(nested, self_hosted);
        assert_eq!(
            self_hosted.to_string(),
            "gitlab:git.example.com/group/sub/repo"
        );
    }
}
