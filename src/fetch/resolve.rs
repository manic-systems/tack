// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    fs,
    io::Read as _,
    path::{
        Path,
        PathBuf,
    },
};

use eyre::{
    ContextCompat as _,
    Result,
    WrapErr as _,
    bail,
};
use ureq::{
    Body,
    ResponseExt as _,
    http as ureq_http,
};

use super::{
    FetchResult,
    archive::{
        detect_tar_format,
        unpack_tar_stream,
    },
    auth::record_fetch_warning,
    forge,
    git,
    github,
    gitlab,
    http::HttpClient,
    time::epoch_from_http_date,
};
use crate::{
    error::user_bail,
    lock::LockedNode,
    nar,
    pins::Unpack,
    source::{
        Source,
        clone_url,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedPin {
    node:     LockedNode,
    identity: FetchIdentity,
}

impl FetchedPin {
    pub(super) const fn rev(node: LockedNode, rev: String) -> Self {
        Self {
            node,
            identity: FetchIdentity::Rev(rev),
        }
    }

    const fn content_hash(node: LockedNode, hash: String) -> Self {
        Self {
            node,
            identity: FetchIdentity::ContentHash(hash),
        }
    }

    const fn immutable_url(node: LockedNode, url: String) -> Self {
        Self {
            node,
            identity: FetchIdentity::ImmutableUrl(url),
        }
    }

    const fn path(node: LockedNode, path: String) -> Self {
        Self {
            node,
            identity: FetchIdentity::Path(path),
        }
    }

    pub fn into_parts(self) -> (LockedNode, FetchIdentity) {
        (self.node, self.identity)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchIdentity {
    Rev(String),
    ContentHash(String),
    ImmutableUrl(String),
    Path(String),
}

impl FetchIdentity {
    pub fn as_str(&self) -> &str {
        match *self {
            Self::Rev(ref value)
            | Self::ContentHash(ref value)
            | Self::ImmutableUrl(ref value)
            | Self::Path(ref value) => value,
        }
    }
}

impl AsRef<str> for FetchIdentity {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<FetchIdentity> for String {
    fn from(identity: FetchIdentity) -> Self {
        match identity {
            FetchIdentity::Rev(value)
            | FetchIdentity::ContentHash(value)
            | FetchIdentity::ImmutableUrl(value)
            | FetchIdentity::Path(value) => value,
        }
    }
}

pub(super) fn current_rev(source: &Source) -> Result<String> {
    match *source {
        Source::Github {
            ref owner,
            ref repo,
            ref reff,
            rev: ref pinned,
        } => github::current_rev(owner, repo, reff.as_deref(), pinned.as_deref()),
        Source::Git { .. } | Source::Gitlab { .. } => {
            let target = source
                .git_target()
                .context("git-backed source missing git target")?;
            if target.rev.is_none()
                && let Some(rev) = forge_resolve_ref(target.url.as_ref(), target.reff)
            {
                return Ok(rev);
            }
            git::current_rev(target.url.as_ref(), target.reff, target.rev)
        },
        Source::Tarball { ref url } => {
            let http = HttpClient::global();
            let resp = http
                .head(url.as_str())
                .call()
                .or_else(|_| http.get(url.as_str()).call().map_err(Box::new))
                .wrap_err_with(|| format!("probe {url}"))?;
            Ok(immutable_url_of(&resp, url))
        },
        Source::Path { ref path } => Ok(path.clone()),
    }
}

fn forge_resolve_ref(url: &str, reff: Option<&str>) -> Option<String> {
    let repo = forge::detect_git_url(url)?;
    forge::resolve_ref(repo.kind, &repo.host, &repo.owner, &repo.repo, reff)
        .ok()
        .flatten()
}

pub fn fetch_fixed_pin(url: &str, unpack: Option<Unpack>) -> Result<FetchedPin> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        user_bail!("fixed pins require a plain http(s) URL, got: {url}");
    }
    let mut resp = HttpClient::global()
        .get(url)
        .call()
        .wrap_err_with(|| format!("GET {url}"))?;
    let immutable_url = immutable_url_of(&resp, url);
    let mut bytes = Vec::new();
    resp.body_mut()
        .as_reader()
        .read_to_end(&mut bytes)
        .wrap_err_with(|| format!("read body of {url}"))?;
    let sha256 = nar::hash_bytes(&bytes);

    // redirected immutable urls may lose the archive extension
    let kind = unpack.unwrap_or_else(|| {
        if Unpack::detect(url) == Unpack::Tarball
            || Unpack::detect(&immutable_url) == Unpack::Tarball
        {
            Unpack::Tarball
        } else {
            Unpack::File
        }
    });
    let node = LockedNode::new_fixed(immutable_url, sha256.clone(), kind.as_str());
    Ok(FetchedPin::content_hash(node, sha256))
}

pub fn fetch_locked_tree_into(node: &LockedNode, dir: &Path) -> Result<PathBuf> {
    match *node {
        LockedNode::Github {
            ref owner,
            ref repo,
            rev: ref locked_rev,
            ..
        } => {
            let resolved_rev = locked_rev.as_deref().context("github node missing rev")?;
            github::fetch_locked_tree_into(owner, repo, resolved_rev, dir)
        },
        LockedNode::Git {
            ref url,
            ref reff,
            ref rev,
            submodules,
            ..
        } => git::fetch_tree_into(url, reff.as_deref(), rev.as_deref(), submodules, dir),
        LockedNode::Tarball { ref url, .. } => {
            let mut resp = HttpClient::global()
                .get(url)
                .call()
                .wrap_err_with(|| format!("GET {url}"))?;
            let format = detect_tar_format(url).wrap_err_with(|| format!("tarball {url}"))?;
            unpack_tar_stream(resp.body_mut().as_reader(), format, dir)
        },
        LockedNode::Gitlab {
            ref host,
            ref owner,
            ref repo,
            rev: ref locked_rev,
            ..
        } => {
            let resolved_rev = locked_rev.as_deref().context("gitlab node missing rev")?;
            gitlab::download_archive(host, owner, repo, resolved_rev, dir)
        },
        LockedNode::Fixed { .. } | LockedNode::Indirect { .. } | LockedNode::Path { .. } => {
            bail!("cannot inspect tree for lock type '{}'", node.kind())
        },
    }
}

pub fn fetch_tree_into(source: &Source, submodules: bool, dir: &Path) -> Result<PathBuf> {
    let resolved = downgrade_forge_for_submodules(source, submodules);
    match *resolved {
        Source::Github {
            ref owner,
            ref repo,
            ref reff,
            rev: ref pinned,
        } => github::fetch_tree_into(owner, repo, reff.as_deref(), pinned.as_deref(), dir),
        Source::Gitlab {
            ref host,
            ref owner,
            ref repo,
            ..
        } => {
            let rev = current_rev(resolved.as_ref())
                .wrap_err_with(|| format!("resolve gitlab ref for {host}/{owner}/{repo}"))?;
            gitlab::download_archive(host, owner, repo, &rev, dir)
        },
        Source::Git { .. } => {
            let target = resolved
                .git_target()
                .context("git-backed source missing git target")?;
            git::fetch_tree_into(
                target.url.as_ref(),
                target.reff,
                target.rev,
                submodules,
                dir,
            )
        },
        Source::Tarball { ref url } => {
            let mut resp = HttpClient::global()
                .get(url.as_str())
                .call()
                .wrap_err_with(|| format!("GET {url}"))?;
            let format = detect_tar_format(url).wrap_err_with(|| format!("tarball {url}"))?;
            unpack_tar_stream(resp.body_mut().as_reader(), format, dir)
        },
        Source::Path { .. } => bail!("cannot fetch a tree for a local path pin"),
    }
}

pub fn fetch_pin(source: &Source, submodules: bool) -> Result<FetchedPin> {
    let resolved = downgrade_forge_for_submodules(source, submodules);
    match *resolved {
        Source::Github {
            ref owner,
            ref repo,
            ref reff,
            rev: ref pinned,
        } => github::fetch_pin(owner, repo, reff.as_deref(), pinned.clone()),
        Source::Gitlab {
            ref host,
            ref owner,
            ref repo,
            ..
        } => fetch_gitlab_archive_pin(resolved.as_ref(), host, owner, repo),
        Source::Git { .. } => {
            let target = resolved
                .git_target()
                .context("git-backed source missing git target")?;
            let checkout =
                git::fetch_pin_checkout(target.url.as_ref(), target.reff, target.rev, submodules)?;
            git_pin_from_checkout(resolved.as_ref(), checkout, submodules)
        },
        Source::Tarball { ref url } => {
            let mut resp = HttpClient::global()
                .get(url.as_str())
                .call()
                .wrap_err_with(|| format!("GET {url}"))?;
            let immutable_url = immutable_url_of(&resp, url);
            let last_modified = resp
                .headers()
                .get("Last-Modified")
                .and_then(|header| header.to_str().ok())
                .and_then(|header| epoch_from_http_date(header).ok())
                .unwrap_or(0);
            let format = detect_tar_format(&immutable_url)
                .or_else(|_| detect_tar_format(url))
                .wrap_err_with(|| format!("tarball {url}"))?;

            let dir = tempfile::tempdir()?;
            let root = unpack_tar_stream(resp.body_mut().as_reader(), format, dir.path())?;
            let nar_hash = nar::hash_path(&root)?;
            // record a shipped rev so fetchTree exposes rev/shortRev
            let node = git_revision_of(&root).map_or_else(
                || LockedNode::new_tarball(immutable_url.clone(), nar_hash.clone(), last_modified),
                |rev| {
                    LockedNode::new_tarball_with_rev(immutable_url.clone(), rev, nar_hash.clone())
                },
            );
            Ok(FetchedPin::immutable_url(node, immutable_url))
        },
        Source::Path { ref path } => {
            let nar_hash = Path::new(path)
                .is_absolute()
                .then(|| nar::hash_path(Path::new(path)))
                .transpose()
                .wrap_err_with(|| format!("hash path pin {path}"))?;
            Ok(FetchedPin::path(
                LockedNode::new_path(path.clone(), nar_hash),
                path.clone(),
            ))
        },
    }
}

/// forge archives cannot represent submodules
fn downgrade_forge_for_submodules(source: &Source, submodules: bool) -> Cow<'_, Source> {
    if !submodules {
        return Cow::Borrowed(source);
    }
    match *source {
        Source::Github {
            ref owner,
            ref repo,
            ref reff,
            ref rev,
        } => {
            Cow::Owned(Source::Git {
                url:  clone_url("github.com", owner, repo),
                reff: reff.clone(),
                rev:  rev.clone(),
            })
        },
        Source::Gitlab {
            ref host,
            ref owner,
            ref repo,
            ref reff,
            ref rev,
        } => {
            Cow::Owned(Source::Git {
                url:  clone_url(host, owner, repo),
                reff: reff.clone(),
                rev:  rev.clone(),
            })
        },
        Source::Git { .. } | Source::Tarball { .. } | Source::Path { .. } => Cow::Borrowed(source),
    }
}

fn fetch_gitlab_archive_pin(
    source: &Source,
    host: &str,
    owner: &str,
    repo: &str,
) -> Result<FetchedPin> {
    let rev = current_rev(source)
        .wrap_err_with(|| format!("resolve gitlab ref for {host}/{owner}/{repo}"))?;
    let dir = tempfile::tempdir()?;
    let root = gitlab::download_archive(host, owner, repo, &rev, dir.path())?;
    let nar_hash = nar::hash_path(&root)?;
    let last_modified =
        gitlab::commit_last_modified(host, owner, repo, &rev).unwrap_or_else(|| {
            record_fetch_warning(format!(
                "could not fetch lastModified for gitlab {host}/{owner}/{repo}@{rev}; using 0"
            ));
            0
        });
    let node = LockedNode::new_gitlab(host, owner, repo, rev.clone(), nar_hash, last_modified);
    Ok(FetchedPin::rev(node, rev))
}

fn git_pin_from_checkout(
    source: &Source,
    checkout: git::PinCheckout,
    submodules: bool,
) -> Result<FetchedPin> {
    let node = match *source {
        Source::Git { ref url, .. } => {
            LockedNode::new_git(
                url,
                checkout.refname,
                checkout.rev.clone(),
                checkout.nar_hash,
                checkout.last_modified,
                submodules,
            )
        },
        Source::Github { .. }
        | Source::Gitlab { .. }
        | Source::Tarball { .. }
        | Source::Path { .. } => {
            bail!("non-git source cannot be locked from git checkout")
        },
    };

    Ok(FetchedPin::rev(node, checkout.rev))
}

fn immutable_url_of(resp: &ureq_http::Response<Body>, fallback: &str) -> String {
    resp.headers()
        .get("Link")
        .and_then(|header| header.to_str().ok())
        .and_then(parse_link_immutable)
        .unwrap_or_else(|| {
            let uri = resp.get_uri().to_string();
            if uri.is_empty() {
                fallback.to_owned()
            } else {
                uri
            }
        })
}

fn parse_link_immutable(header: &str) -> Option<String> {
    for raw_part in header.split(',') {
        let part = raw_part.trim();
        let Some((url_part, params)) = part.split_once(';') else {
            continue;
        };
        let Some(url) = url_part
            .trim()
            .strip_prefix('<')
            .and_then(|inner| inner.strip_suffix('>'))
        else {
            continue;
        };
        for param in params.split(';') {
            let Some((key, raw_value)) = param.trim().split_once('=') else {
                continue;
            };
            if key.trim().eq_ignore_ascii_case("rel") {
                let rel = raw_value.trim().trim_matches('"');
                if rel == "immutable" || rel == "immutable_link" {
                    return Some(url.to_owned());
                }
            }
        }
    }
    None
}

/// embedded `.git-revision`, if present and a plausible git object id
fn git_revision_of(root: &Path) -> Option<String> {
    let contents = fs::read_to_string(root.join(".git-revision")).ok()?;
    let rev = contents.trim();
    let looks_like_rev =
        (7..=64).contains(&rev.len()) && rev.bytes().all(|byte| byte.is_ascii_hexdigit());
    looks_like_rev.then(|| rev.to_owned())
}

pub fn raw(url: &str) -> FetchResult<String> {
    HttpClient::global().raw_text(url)
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
