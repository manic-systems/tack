// SPDX-License-Identifier: EUPL-1.2

use std::{
    fmt::{
        Display,
        Formatter,
        Result as FmtResult,
    },
    path::Path,
};

use data_encoding::HEXLOWER;
use hmac_sha256::Hash as Sha256;
use misstep::{
    Result,
    ResultExt as _,
};

use crate::{
    error::user_bail,
    fetch,
    shorturl::ShortUrls,
    source::split_query_fragment,
};

/// one entry of a pin's `patches`, displayed as the text it was written as,
/// since the lock and the resolver match entries by that text
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchSource {
    /// a file relative to the `.tack` dir
    Local(String),
    Remote(RemotePatch),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemotePatch {
    /// what `pins.toml` says, which may be a shorturl
    written: String,
    url:     String,
    kind:    Remote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Remote {
    Pull { repo: ForgeRepo, number: u64 },
    Commit { repo: ForgeRepo, rev: String },
    Plain,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeRepo {
    forge: Forge,
    /// every path segment before the repo, so gitlab subgroups stay intact
    owner: String,
    repo:  String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Forge {
    Github,
    Gitlab {
        host: String,
    },
    /// Forgejo and Gitea share one url layout and api
    Gitea {
        host: String,
    },
}

impl PatchSource {
    pub fn parse(raw: &str, shorturls: &ShortUrls<'_>) -> Result<Self> {
        let Some(url) = Self::remote_url(raw, shorturls)? else {
            if Path::new(raw).is_absolute() {
                user_bail!("patch {raw} must be relative to the .tack dir");
            }
            return Ok(Self::Local(raw.to_owned()));
        };
        if !url.starts_with("https://") {
            user_bail!("patch {raw} must come over https");
        }
        Ok(Self::Remote(RemotePatch {
            written: raw.to_owned(),
            kind: Remote::from_url(&url)?,
            url,
        }))
    }

    /// the url `raw` names after shorturl expansion, [`None`] for a local file
    pub fn remote_url(raw: &str, shorturls: &ShortUrls<'_>) -> Result<Option<String>> {
        let expanded = shorturls.expand_aliases(raw)?;
        let url = expanded
            .strip_prefix("github:")
            .map(|path| format!("https://github.com/{path}"))
            .or_else(|| {
                expanded
                    .strip_prefix("gitlab:")
                    .map(|path| format!("https://gitlab.com/{path}"))
            })
            .or_else(|| expanded.strip_prefix("git+").map(str::to_owned))
            .unwrap_or(expanded);
        Ok((url.contains("://") || url != raw).then_some(url))
    }

    pub fn vendored_file(&self, pin: &str) -> Option<String> {
        match *self {
            Self::Local(_) => None,
            Self::Remote(ref remote) => Some(remote.vendored_file(pin)),
        }
    }
}

impl Display for PatchSource {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match *self {
            Self::Local(ref path) => f.write_str(path),
            Self::Remote(ref remote) => f.write_str(&remote.written),
        }
    }
}

impl Remote {
    fn from_url(url: &str) -> Result<Self> {
        let Some((authority, path)) = url
            .strip_prefix("https://")
            .and_then(|rest| rest.split_once('/'))
        else {
            return Ok(Self::Plain);
        };
        let lowered = authority.to_ascii_lowercase();
        let bare = lowered.strip_suffix(":443").unwrap_or(&lowered);
        let host = bare.strip_prefix("www.").unwrap_or(bare);
        let (route_path, query) = split_query_fragment(path);
        let mut segments = route_path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if let Some(last) = segments.last_mut() {
            *last = last
                .strip_suffix(".diff")
                .or_else(|| last.strip_suffix(".patch"))
                .unwrap_or(last);
        }
        let pull = |repo: ForgeRepo, raw_number: &str| {
            let parsed = raw_number
                .bytes()
                .all(|byte| byte.is_ascii_digit())
                .then(|| raw_number.parse::<u64>().ok())
                .flatten();
            let Some(number) = parsed else {
                user_bail!("{url}: '{raw_number}' is not a pull request number");
            };
            Ok(Self::Pull { repo, number })
        };
        let commit = |repo: ForgeRepo, rev: &str| {
            Ok(Self::Commit {
                repo,
                rev: rev.to_owned(),
            })
        };

        // gitlab puts its routes after a `-` segment, since projects nest
        if let Some(dash) = segments.iter().position(|&segment| segment == "-") {
            let (project, route) = segments.split_at(dash);
            let Some((&name, owner)) = project.split_last() else {
                return Ok(Self::Plain);
            };
            if owner.is_empty() {
                return Ok(Self::Plain);
            }
            let repo = ForgeRepo::new(
                Forge::Gitlab {
                    host: host.to_owned(),
                },
                &owner.join("/"),
                name,
            );
            // a single commit of a merge request is named in the query
            let picked = query.and_then(|params| {
                params
                    .split('&')
                    .find_map(|param| param.strip_prefix("commit_id="))
            });
            return match (route, picked) {
                (&["-", "merge_requests", _, "diffs", ..], Some(rev))
                | (&["-", "commit", rev, ..], _) => commit(repo, rev),
                (&["-", "merge_requests", number, ..], _) => pull(repo, number),
                _ => Ok(Self::Plain),
            };
        }

        let forge = if host == "github.com" {
            Forge::Github
        } else {
            Forge::Gitea {
                host: host.to_owned(),
            }
        };
        match (forge, segments.as_slice()) {
            (
                Forge::Github,
                &([owner, repo, "pull", _, "commits", rev, ..] | [owner, repo, "commit", rev, ..]),
            ) => commit(ForgeRepo::new(Forge::Github, owner, repo), rev),
            (Forge::Github, &[owner, repo, "pull", number, ..]) => {
                pull(ForgeRepo::new(Forge::Github, owner, repo), number)
            },
            (
                gitea @ Forge::Gitea { .. },
                &([owner, repo, "pulls", _, "commits", rev, ..] | [owner, repo, "commit", rev, ..]),
            ) => commit(ForgeRepo::new(gitea, owner, repo), rev),
            (gitea @ Forge::Gitea { .. }, &[owner, repo, "pulls", number, ..]) => {
                pull(ForgeRepo::new(gitea, owner, repo), number)
            },
            _ => Ok(Self::Plain),
        }
    }
}

impl ForgeRepo {
    fn new(forge: Forge, owner: &str, repo: &str) -> Self {
        Self {
            forge,
            owner: owner.to_owned(),
            repo: repo.to_owned(),
        }
    }

    fn pull_diff(&self, number: u64) -> Result<String> {
        let Self {
            ref owner,
            ref repo,
            ..
        } = *self;
        Ok(match self.forge {
            Forge::Github => fetch::github::pull_diff(owner, repo, number)?,
            Forge::Gitlab { ref host } => {
                fetch::raw(
                    &format!("https://{host}/{owner}/{repo}/-/merge_requests/{number}.diff"),
                    None,
                )?
            },
            Forge::Gitea { ref host } => {
                fetch::raw(
                    &format!("https://{host}/{owner}/{repo}/pulls/{number}.diff"),
                    None,
                )?
            },
        })
    }

    fn commit_diff(&self, rev: &str) -> Result<String> {
        let Self {
            ref owner,
            ref repo,
            ..
        } = *self;
        Ok(match self.forge {
            Forge::Github => fetch::github::commit_diff(owner, repo, rev)?,
            Forge::Gitlab { ref host } => {
                fetch::raw(
                    &format!("https://{host}/{owner}/{repo}/-/commit/{rev}.diff"),
                    None,
                )?
            },
            Forge::Gitea { ref host } => {
                fetch::raw(
                    &format!("https://{host}/{owner}/{repo}/commit/{rev}.diff"),
                    None,
                )?
            },
        })
    }
}

/// whether the url's path ends in a `.patch` or `.diff` file
fn serves_diff(url: &str) -> bool {
    let path = split_query_fragment(url).0;
    let last = path.rsplit('/').next().unwrap_or_default();
    Path::new(last)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("patch") || ext.eq_ignore_ascii_case("diff"))
}

impl RemotePatch {
    fn file_name(&self) -> String {
        match self.kind {
            Remote::Pull {
                ref repo, number, ..
            } => {
                match repo.forge {
                    Forge::Gitlab { .. } => format!("mr-{number}.patch"),
                    Forge::Github | Forge::Gitea { .. } => format!("pr-{number}.patch"),
                }
            },
            Remote::Commit { ref rev, .. } => format!("{}.patch", rev.get(..12).unwrap_or(rev)),
            Remote::Plain => {
                let (path, query) = split_query_fragment(&self.url);
                let last = path.rsplit('/').next().unwrap_or_default();
                if serves_diff(&self.url) {
                    last.to_owned()
                } else if last.is_empty() || query.is_some() {
                    // cgit's `patch/?id=` and download scripts name nothing in
                    // the path, so the url itself has to tell them apart
                    let digest = HEXLOWER.encode(&Sha256::hash(self.url.as_bytes()));
                    format!("{}.patch", digest.get(..12).unwrap_or(&digest))
                } else {
                    format!("{last}.patch")
                }
            },
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn vendored_file(&self, pin: &str) -> String {
        format!("patches/{pin}/{}", self.file_name())
    }

    pub fn download(&self) -> Result<Vec<u8>> {
        let fetched = self.fetch();
        let anonymous = matches!(
            self.kind,
            Remote::Pull { ref repo, .. } | Remote::Commit { ref repo, .. }
                if matches!(repo.forge, Forge::Github)
        ) && !fetch::has_token("github.com");
        if anonymous {
            return fetched.context(
                "github answers a private repo as missing without a token, set GITHUB_TOKEN or \
                 GH_TOKEN if it is one",
            );
        }
        fetched
    }

    fn fetch(&self) -> Result<Vec<u8>> {
        let text = match self.kind {
            // a url that already serves a diff is fetched as written, since a
            // host that merely looks like gitea may not have its routes
            Remote::Pull { ref repo, .. } | Remote::Commit { ref repo, .. }
                if !matches!(repo.forge, Forge::Github) && serves_diff(&self.url) =>
            {
                fetch::raw(&self.url, None)?
            },
            Remote::Pull { ref repo, number } => repo.pull_diff(number)?,
            Remote::Commit { ref repo, ref rev } => repo.commit_diff(rev)?,
            Remote::Plain => fetch::raw(&self.url, None)?,
        };
        Ok(text.into_bytes())
    }
}
