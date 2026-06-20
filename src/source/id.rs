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

// github has no subgroups; the contains('/') guard rejects nested paths
fn github_parse_git_url(url: &str) -> Option<git_url::RepoRef> {
    git_url::parse(url).filter(|repo| repo.host == "github.com" && !repo.owner.contains('/'))
}

fn classify_git_url(url: &str) -> SourceId {
    if let Some(repo) = gitlab::parse_git_url(url) {
        return SourceId::gitlab(&repo.host, &repo.owner, &repo.repo);
    }
    if let Some(repo) = github_parse_git_url(url) {
        return SourceId::github(&repo.owner, &repo.repo);
    }
    SourceId::Git {
        url: url.to_lowercase(),
    }
}

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

impl From<Source> for SourceId {
    fn from(source: Source) -> Self {
        match source {
            Source::Github { owner, repo, .. } => Self::github(&owner, &repo),
            Source::Gitlab {
                host, owner, repo, ..
            } => Self::gitlab(&host, &owner, &repo),
            Source::Git { url, .. } => classify_git_url(&url),
            Source::Tarball { url } => {
                Self::Tarball {
                    url: strip_query_fragment(&url).to_lowercase(),
                }
            },
            Source::Path { path } => {
                Self::Path {
                    path: path.to_lowercase(),
                }
            },
        }
    }
}

impl SourceId {
    pub fn from_url(expanded: &str) -> Option<Self> {
        expanded.parse::<Source>().ok().map(Self::from)
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
            LockedNode::Git { ref url, .. } => Some(classify_git_url(strip_query_fragment(url))),
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
#[path = "id_tests.rs"]
mod tests;
