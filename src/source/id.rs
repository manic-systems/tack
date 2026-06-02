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
        gitlab,
        host,
        strip_query_fragment,
    },
};

/// canonical identity of a pin source, from a url or a locked node
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
    /// git+url, query string stripped
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
                Self::Git {
                    url: url.to_lowercase(),
                }
            },
            Source::Tarball { ref url } => {
                Self::Tarball {
                    url: strip_query_fragment(url).to_lowercase(),
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
    fn from_url_parses_each_scheme() {
        assert_eq!(
            SourceId::from_url("github:NixOS/nixpkgs/nixos-unstable")
                .unwrap()
                .to_string(),
            "github:nixos/nixpkgs"
        );
        assert_eq!(
            SourceId::from_url("gitlab:NixOS/nixpkgs/nixos-unstable")
                .unwrap()
                .to_string(),
            "gitlab:gitlab.com/nixos/nixpkgs"
        );
        assert_eq!(
            SourceId::from_url("gitlab:NixOS/nixpkgs?host=Git.Example.Com")
                .unwrap()
                .to_string(),
            "gitlab:git.example.com/nixos/nixpkgs"
        );
        assert_eq!(
            SourceId::from_url("git+https://x.com/o/r?ref=main")
                .unwrap()
                .to_string(),
            "git+https://x.com/o/r"
        );
        assert_eq!(
            SourceId::from_url("git+https://gitlab.com/NixOS/nixpkgs.git?ref=main")
                .unwrap()
                .to_string(),
            "gitlab:gitlab.com/nixos/nixpkgs"
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
    fn gitlab_url_and_node_agree_for_default_host() {
        let from_flake = SourceId::from_url("gitlab:NixOS/nixpkgs/nixos-unstable").unwrap();
        let from_git =
            SourceId::from_url("git+https://gitlab.com/NixOS/nixpkgs.git?ref=main").unwrap();
        let from_git_node = SourceId::from_locked(&node(
            json!({"type": "git", "url": "https://gitlab.com/NixOS/nixpkgs.git?ref=main"}),
        ))
        .unwrap();
        let from_node = SourceId::from_locked(&node(
            json!({"type": "gitlab", "owner": "NixOS", "repo": "nixpkgs"}),
        ))
        .unwrap();

        assert_eq!(from_flake, from_node);
        assert_eq!(from_git, from_node);
        assert_eq!(from_git_node, from_node);
    }

    #[test]
    fn gitlab_url_and_node_agree_for_self_hosted() {
        let from_flake =
            SourceId::from_url("gitlab:NixOS/nixpkgs?host=GitLab.Example.Com").unwrap();
        let from_git =
            SourceId::from_url("git+https://gitlab.example.com/NixOS/nixpkgs.git").unwrap();
        let from_node = SourceId::from_locked(&node(
            json!({"type": "gitlab", "host": "gitlab.example.com", "owner": "NixOS", "repo": "nixpkgs"}),
        ))
        .unwrap();

        assert_eq!(from_flake, from_node);
        assert_eq!(from_git, from_node);
    }

    #[test]
    fn gitlab_identity_preserves_non_default_host_port() {
        let from_flake =
            SourceId::from_url("gitlab:NixOS/nixpkgs?host=GitLab.Example.Com:8443").unwrap();
        let from_git = SourceId::from_url(
            "git+https://GitLab.Example.Com:8443/NixOS/nixpkgs.git?ref=main#frag",
        )
        .unwrap();
        let from_node = SourceId::from_locked(&node(
            json!({"type": "gitlab", "host": "GitLab.Example.Com:8443", "owner": "NixOS", "repo": "nixpkgs"}),
        ))
        .unwrap();

        assert_eq!(
            from_flake.to_string(),
            "gitlab:gitlab.example.com:8443/nixos/nixpkgs"
        );
        assert_eq!(from_flake, from_git);
        assert_eq!(from_flake, from_node);
    }

    #[test]
    fn gitlab_url_decodes_nested_group_owner() {
        assert_eq!(
            SourceId::from_url("gitlab:Veloren%2Fdev/rfcs")
                .unwrap()
                .to_string(),
            SourceId::from_locked(&node(
                json!({"type": "gitlab", "owner": "veloren/dev", "repo": "rfcs"}),
            ))
            .unwrap()
            .to_string()
        );
    }

    #[test]
    fn gitlab_git_urls_cover_nested_groups_and_ssh_forms() {
        let first_class = SourceId::from_url("gitlab:group%2Fsub/repo").unwrap();
        let https = SourceId::from_url("git+https://gitlab.com/group/sub/repo.git").unwrap();
        let ssh = SourceId::from_url("git+ssh://git@gitlab.com:2222/group/sub/repo.git").unwrap();
        let scp = SourceId::from_url("git+git@gitlab.com:group/sub/repo.git").unwrap();
        let locked_git = SourceId::from_locked(&node(
            json!({"type": "git", "url": "ssh://git@gitlab.com:2222/group/sub/repo.git?ref=main#frag"}),
        ))
        .unwrap();

        assert_eq!(first_class, https);
        assert_eq!(first_class, ssh);
        assert_eq!(first_class, scp);
        assert_eq!(first_class, locked_git);
    }

    #[test]
    fn gitlab_source_id_normalizes_default_ports_by_url_scheme() {
        let default_http =
            SourceId::from_url("git+http://GitLab.Example.Com:80/o/r.git?ref=main#frag").unwrap();
        let default_https =
            SourceId::from_url("git+https://GitLab.Example.Com:443/o/r.git?ref=main#frag").unwrap();
        let https_non_default =
            SourceId::from_url("git+https://GitLab.Example.Com:80/o/r.git?ref=main#frag").unwrap();

        assert_eq!(default_http.to_string(), "gitlab:gitlab.example.com/o/r");
        assert_eq!(default_https.to_string(), "gitlab:gitlab.example.com/o/r");
        assert_eq!(
            https_non_default.to_string(),
            "gitlab:gitlab.example.com:80/o/r"
        );
    }

    #[test]
    fn gitlab_locked_node_identity_includes_host() {
        let default_host = SourceId::from_locked(&node(
            json!({"type": "gitlab", "owner": "NixOS", "repo": "Nixpkgs"}),
        ))
        .unwrap();
        let explicit_default_host = SourceId::from_locked(&node(
            json!({"type": "gitlab", "host": "GITLAB.COM", "owner": "nixos", "repo": "nixpkgs"}),
        ))
        .unwrap();
        let self_hosted = SourceId::from_locked(&node(
            json!({"type": "gitlab", "host": "Git.Example.Com", "owner": "NixOS", "repo": "Nixpkgs"}),
        ))
        .unwrap();

        assert_eq!(default_host.to_string(), "gitlab:gitlab.com/nixos/nixpkgs");
        assert_eq!(default_host, explicit_default_host);
        assert_eq!(
            self_hosted.to_string(),
            "gitlab:git.example.com/nixos/nixpkgs"
        );
        assert_ne!(default_host, self_hosted);
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
