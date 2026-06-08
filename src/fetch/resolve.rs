// SPDX-License-Identifier: EUPL-1.2

use std::{
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
    http::HttpClient,
    time::epoch_from_http_date,
};
use crate::{
    lock::LockedNode,
    nar,
    pins::Unpack,
    source::{
        Source,
        gitlab as source_gitlab,
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
        // a path pin is read locally at eval time; nothing to compare upstream
        Source::Path { ref path } => Ok(path.clone()),
    }
}

fn forge_resolve_ref(url: &str, reff: Option<&str>) -> Option<String> {
    let repo = forge::detect_git_url(url)?;
    forge::resolve_ref(repo.kind, &repo.host, &repo.owner, &repo.repo, reff)
        .ok()
        .flatten()
}

/// fetch a fixed pin: sha256 the raw bytes (not nar), return the node plus that
/// hash as the drift-display rev. unpack auto-detected from the url if not
/// given
pub fn fetch_fixed_pin(url: &str, unpack: Option<Unpack>) -> Result<(LockedNode, String)> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        bail!("fixed pins require a plain http(s) URL, got: {url}");
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

    // user url first: the immutable url may have lost its extension to a redirect
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
            let url = source_gitlab::clone_url(host, owner, repo);
            git::fetch_tree_rev_into(&url, resolved_rev, false, dir)
        },
        LockedNode::Fixed { .. } | LockedNode::Indirect { .. } | LockedNode::Path { .. } => {
            bail!("cannot inspect tree for lock type '{}'", node.kind())
        },
    }
}

pub fn fetch_tree_into(source: &Source, submodules: bool, dir: &Path) -> Result<PathBuf> {
    match *source {
        Source::Github {
            ref owner,
            ref repo,
            ref reff,
            rev: ref pinned,
        } => github::fetch_tree_into(owner, repo, reff.as_deref(), pinned.as_deref(), dir),
        Source::Git { .. } | Source::Gitlab { .. } => {
            reject_gitlab_submodules(source, submodules)?;
            let target = source
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
    match *source {
        Source::Github {
            ref owner,
            ref repo,
            ref reff,
            rev: ref pinned,
        } => github::fetch_pin(owner, repo, reff.as_deref(), pinned.clone()),
        Source::Git { .. } | Source::Gitlab { .. } => {
            reject_gitlab_submodules(source, submodules)?;
            let target = source
                .git_target()
                .context("git-backed source missing git target")?;
            let checkout =
                git::fetch_pin_checkout(target.url.as_ref(), target.reff, target.rev, submodules)?;
            git_pin_from_checkout(source, checkout, submodules)
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
        // a path pin locks to its spec with no fetch; the resolver reads the
        // directory at nix eval time
        Source::Path { ref path } => Ok((LockedNode::new_path(path.clone()), path.clone())),
    }
}

fn reject_gitlab_submodules(source: &Source, submodules: bool) -> Result<()> {
    if submodules && matches!(source, Source::Gitlab { .. }) {
        bail!("gitlab sources do not support submodules; use a git+ URL for submodule pins");
    }
    Ok(())
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
        Source::Gitlab {
            ref host,
            ref owner,
            ref repo,
            ..
        } => {
            LockedNode::new_gitlab(
                host,
                owner,
                repo,
                rev.clone(),
                checkout.nar_hash,
                checkout.last_modified,
            )
        },
        Source::Github { .. } | Source::Tarball { .. } | Source::Path { .. } => {
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

/// extract the immutable url from a Link header (rfc 8288)
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
        fetch_locked_tree_into,
        git,
        git_pin_from_checkout,
        parse_link_immutable,
        reject_gitlab_submodules,
    };
    use crate::{
        lock::LockedNode,
        source::Source,
    };

    fn node(value: serde_json::Value) -> LockedNode {
        LockedNode::from_value(value).unwrap()
    }

    #[test]
    fn link_header_immutable() {
        let immutable = "<https://releases.nixos.org/nixos/abc/nixexprs.tar.xz>; rel=\"immutable\"";
        assert_eq!(
            parse_link_immutable(immutable).as_deref(),
            Some("https://releases.nixos.org/nixos/abc/nixexprs.tar.xz")
        );

        // rel=immutable_link is an older name some nix releases use
        let immutable_link = "<https://x/y>; rel=\"immutable_link\"";
        assert_eq!(
            parse_link_immutable(immutable_link).as_deref(),
            Some("https://x/y")
        );

        // no immutable rel: no answer, not the wrong url
        let canonical = "<https://x/y>; rel=\"canonical\"";
        assert!(parse_link_immutable(canonical).is_none());

        // immutable wins regardless of position
        let mixed = "<https://x/canon>; rel=\"canonical\", <https://x/imm>; rel=\"immutable\"";
        assert_eq!(
            parse_link_immutable(mixed).as_deref(),
            Some("https://x/imm")
        );
    }

    #[test]
    fn gitlab_locked_tree_fetch_requires_locked_rev() {
        let tmp = tempfile::tempdir().unwrap();
        let err = fetch_locked_tree_into(
            &node(json!({
                "type": "gitlab",
                "host": "git.example.com",
                "owner": "o",
                "repo": "r"
            })),
            tmp.path(),
        )
        .unwrap_err();

        assert_eq!(err.to_string(), "gitlab node missing rev");
    }

    #[test]
    fn gitlab_source_checkout_locks_as_gitlab_node() {
        let source = "gitlab:Group/Repo/main?host=gitlab.example.com&rev=abc123"
            .parse::<Source>()
            .unwrap();
        let (fetched, rev) = git_pin_from_checkout(
            &source,
            git::PinCheckout {
                rev:           "abc123".to_owned(),
                nar_hash:      "sha256-n".to_owned(),
                last_modified: 1700,
                refname:       "refs/heads/main".to_owned(),
            },
            true,
        )
        .unwrap();

        assert_eq!(rev, "abc123");
        assert_eq!(
            fetched,
            node(json!({
                "type": "gitlab",
                "host": "gitlab.example.com",
                "owner": "Group",
                "repo": "Repo",
                "rev": "abc123",
                "narHash": "sha256-n",
                "lastModified": 1_700_i64
            }))
        );
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

    #[test]
    fn first_class_gitlab_sources_reject_submodules() {
        let source = "gitlab:Group/Repo/main".parse::<Source>().unwrap();

        let err = reject_gitlab_submodules(&source, true).unwrap_err();

        assert!(
            err.to_string()
                .contains("gitlab sources do not support submodules")
        );
    }
}
