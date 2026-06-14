// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
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

pub fn current_rev(source: &Source) -> Result<String> {
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

pub fn fetch_fixed_pin(url: &str, unpack: Option<Unpack>) -> Result<(LockedNode, String)> {
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
    Ok((node, sha256))
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

pub fn fetch_pin(source: &Source, submodules: bool) -> Result<(LockedNode, String)> {
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
            let node = LockedNode::new_tarball(immutable_url.clone(), nar_hash, last_modified);
            Ok((node, immutable_url))
        },
        Source::Path { ref path } => {
            let nar_hash = Path::new(path)
                .is_absolute()
                .then(|| nar::hash_path(Path::new(path)))
                .transpose()
                .wrap_err_with(|| format!("hash path pin {path}"))?;
            Ok((LockedNode::new_path(path.clone(), nar_hash), path.clone()))
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
) -> Result<(LockedNode, String)> {
    let rev = current_rev(source)
        .wrap_err_with(|| format!("resolve gitlab ref for {host}/{owner}/{repo}"))?;
    let dir = tempfile::tempdir()?;
    let root = gitlab::download_archive(host, owner, repo, &rev, dir.path())?;
    let nar_hash = nar::hash_path(&root)?;
    let last_modified = gitlab::commit_last_modified(host, owner, repo, &rev).unwrap_or(0);
    let node = LockedNode::new_gitlab(host, owner, repo, rev.clone(), nar_hash, last_modified);
    Ok((node, rev))
}

fn git_pin_from_checkout(
    source: &Source,
    checkout: git::PinCheckout,
    submodules: bool,
) -> Result<(LockedNode, String)> {
    let rev = checkout.rev.clone();
    let node = match *source {
        Source::Git { ref url, .. } => {
            LockedNode::new_git(
                url,
                checkout.refname,
                rev.clone(),
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

    Ok((node, rev))
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
        let (url_part, params) = part.split_once(';')?;
        let url = url_part
            .trim()
            .strip_prefix('<')
            .and_then(|inner| inner.strip_suffix('>'))?;
        for param in params.split(';') {
            let (key, raw_value) = param.trim().split_once('=')?;
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

pub fn raw(url: &str) -> FetchResult<String> {
    HttpClient::global().raw_text(url)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        fetch_pin,
        git,
        git_pin_from_checkout,
    };
    use crate::{
        lock::LockedNode,
        source::Source,
    };

    fn node(value: serde_json::Value) -> LockedNode {
        LockedNode::from_value(value).unwrap()
    }

    #[test]
    fn path_pin_locks_absolute_targets_with_a_nar_hash() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("f"), "hello").unwrap();

        let absolute = Source::Path {
            path: tmp.path().to_string_lossy().into_owned(),
        };
        let (absolute_locked, _) = fetch_pin(&absolute, false).unwrap();
        assert!(
            absolute_locked
                .hash()
                .is_some_and(|hash| hash.starts_with("sha256-"))
        );

        let relative = Source::Path {
            path: "../vendor/dep".to_owned(),
        };
        let (relative_locked, _) = fetch_pin(&relative, false).unwrap();
        assert_eq!(relative_locked.hash(), None);
    }

    #[test]
    fn gitlab_git_url_checkout_stays_generic_git_lock() {
        let source = "git+https://gitlab.com/Group/Repo.git?ref=main&rev=abc123"
            .parse::<Source>()
            .unwrap();
        let (fetched, _) = git_pin_from_checkout(
            &source,
            git::PinCheckout {
                rev:           "abc123".to_owned(),
                nar_hash:      "sha256-n".to_owned(),
                last_modified: 1_700,
                refname:       "refs/heads/main".to_owned(),
            },
            true,
        )
        .unwrap();

        assert_eq!(
            fetched,
            node(json!({
                "type": "git",
                "url": "https://gitlab.com/Group/Repo.git",
                "ref": "refs/heads/main",
                "rev": "abc123",
                "narHash": "sha256-n",
                "lastModified": 1_700_i64,
                "submodules": true
            }))
        );
    }
}
