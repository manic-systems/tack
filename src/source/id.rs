// SPDX-License-Identifier: EUPL-1.2

use std::{
    cmp::Ordering,
    fmt::{
        self,
        Display,
    },
};

use crate::lock::LockedNode;

/// canonical identity of a pin source
/// parsed from either an expanded url or a locked node
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum SourceId {
    Github {
        owner: String,
        repo:  String,
    },
    /// a git+url source, query string stripped
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

impl SourceId {
    /// identity of an expanded pins.toml url
    pub fn from_url(expanded: &str) -> Option<Self> {
        let path = strip_query_fragment(expanded);
        if let Some(body) = path.strip_prefix("github:") {
            let mut segs = body.split('/');
            let owner = segs.next().filter(|segment| !segment.is_empty())?;
            let repo = segs.next().filter(|segment| !segment.is_empty())?;
            return Some(Self::github(owner, repo));
        }
        if let Some(rest) = path.strip_prefix("git+") {
            return Some(Self::Git {
                url: rest.to_lowercase(),
            });
        }
        if path.starts_with("http://") || path.starts_with("https://") {
            return Some(Self::Tarball {
                url: path.to_lowercase(),
            });
        }
        None
    }
    pub fn from_locked(node: &LockedNode) -> Option<Self> {
        match *node {
            LockedNode::Github {
                ref owner,
                ref repo,
                ..
            } => Some(Self::github(owner, repo)),
            LockedNode::Git { ref url, .. } => {
                let cut = strip_query_fragment(url);
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
            LockedNode::Gitlab { .. } | LockedNode::Fixed { .. } => None,
        }
    }

    fn github(owner: &str, repo: &str) -> Self {
        Self::Github {
            owner: owner.to_lowercase(),
            repo:  repo.to_lowercase(),
        }
    }

    /// the github owner/repo, when this id is a github source
    pub fn github_parts(&self) -> Option<(&str, &str)> {
        match *self {
            Self::Github {
                ref owner,
                ref repo,
            } => Some((owner, repo)),
            Self::Git { .. } | Self::Tarball { .. } | Self::Indirect { .. } | Self::Path { .. } => {
                None
            },
        }
    }
}

fn strip_query_fragment(value: &str) -> &str {
    let query = value.find('?').unwrap_or(value.len());
    let fragment = value.find('#').unwrap_or(value.len());
    value.get(..query.min(fragment)).unwrap_or(value)
}

impl Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Github {
                ref owner,
                ref repo,
            } => write!(f, "github:{owner}/{repo}"),
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
    fn from_url_parses_each_scheme() {
        assert_eq!(
            SourceId::from_url("github:NixOS/nixpkgs/nixos-unstable")
                .unwrap()
                .to_string(),
            "github:nixos/nixpkgs"
        );
        assert_eq!(
            SourceId::from_url("git+https://x.com/o/r?ref=main")
                .unwrap()
                .to_string(),
            "git+https://x.com/o/r"
        );
        assert_eq!(
            SourceId::from_url("https://x.com/a.tar.gz")
                .unwrap()
                .to_string(),
            "tarball:https://x.com/a.tar.gz"
        );
        assert!(SourceId::from_url("weird:thing").is_none());
    }

    #[test]
    fn from_node_parses_each_type() {
        assert_eq!(
            SourceId::from_locked(&node(
                json!({"type": "github", "owner": "NixOS", "repo": "Nixpkgs"})
            ))
            .unwrap()
            .to_string(),
            "github:nixos/nixpkgs"
        );
        assert_eq!(
            SourceId::from_locked(&node(
                json!({"type": "git", "url": "https://x/o/r?ref=main"})
            ))
            .unwrap()
            .to_string(),
            "git+https://x/o/r"
        );
        assert_eq!(
            SourceId::from_locked(&node(json!({"type": "indirect", "id": "nixpkgs"})))
                .unwrap()
                .to_string(),
            "indirect:nixpkgs"
        );
        assert_eq!(
            SourceId::from_locked(&node(json!({"type": "path", "path": "/p"})))
                .unwrap()
                .to_string(),
            "path:/p"
        );
    }

    #[test]
    fn url_and_node_strip_query_and_fragment_consistently() {
        assert_eq!(
            SourceId::from_url("git+https://x/o/r?ref=main#frag")
                .unwrap()
                .to_string(),
            SourceId::from_locked(&node(
                json!({"type": "git", "url": "https://x/o/r?ref=main#frag"})
            ))
            .unwrap()
            .to_string()
        );
        assert_eq!(
            SourceId::from_url("https://x/archive.tar.gz#frag")
                .unwrap()
                .to_string(),
            SourceId::from_locked(&node(
                json!({"type": "tarball", "url": "https://x/archive.tar.gz#frag"})
            ))
            .unwrap()
            .to_string()
        );
    }

    #[test]
    fn url_and_node_agree_for_github_case_insensitively() {
        let from_url = SourceId::from_url("github:nixos/nixpkgs").unwrap();
        let from_node = SourceId::from_locked(&node(
            json!({"type": "github", "owner": "NixOS", "repo": "nixpkgs"}),
        ))
        .unwrap();
        assert_eq!(from_url.to_string(), from_node.to_string());
    }

    #[test]
    fn ord_agrees_with_display() {
        let mut ids = [
            SourceId::from_url("github:o/b").unwrap(),
            SourceId::from_url("git+https://x/a").unwrap(),
            SourceId::from_url("github:o/a").unwrap(),
        ];
        ids.sort();
        let by_string = {
            let mut sorted = ids.clone();
            sorted.sort_by_key(ToString::to_string);
            sorted
        };
        assert_eq!(ids, by_string);
    }
}
