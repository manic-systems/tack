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

mod archive;
mod git;
pub mod github;
pub mod gitlab;
pub mod http;
mod time;

use archive::{
    detect_tar_format,
    unpack_tar_stream,
};
use github::{
    BranchComparison,
    CompareStatus,
    CurrentRev,
};
use http::{
    FetchResult,
    HttpClient,
};
use time::epoch_from_http_date;

use crate::{
    lock::LockedNode,
    nar,
    pins::Unpack,
    source::Source,
};

/// upstream rev plus branch topology against the old revision when possible
pub fn current_rev_compared(source: &Source, old_rev: Option<&str>) -> Result<CurrentRev> {
    match *source {
        Source::Github {
            ref owner,
            ref repo,
            ref reff,
            rev: ref pinned,
        } => github::current_rev_compared(owner, repo, reff.as_deref(), pinned.as_deref(), old_rev),
        Source::Git { .. } => {
            let target = source
                .git_target()
                .context("git-backed source missing git target")?;
            git::current_rev_compared(target.url.as_ref(), target.reff, target.rev, old_rev)
        },
        Source::Gitlab {
            ref host,
            ref owner,
            ref repo,
            rev: ref pinned,
            ..
        } => {
            let target = source
                .git_target()
                .context("git-backed source missing git target")?;
            // ls-remote resolves the rev (and peels tags); a pinned rev never
            // moves, so only a resolved moving ref gets a directional compare
            let current =
                git::current_rev_compared(target.url.as_ref(), target.reff, target.rev, old_rev)?;
            let comparison = if pinned.is_some() {
                current.comparison
            } else {
                gitlab::refine_comparison(
                    host,
                    owner,
                    repo,
                    old_rev,
                    &current.rev,
                    current.comparison,
                )
            };
            Ok(CurrentRev {
                rev: current.rev,
                comparison,
            })
        },
        Source::Tarball { ref url } => {
            let http = HttpClient::global();
            let resp = http
                .head(url.as_str())
                .call()
                .or_else(|_| http.get(url.as_str()).call().map_err(Box::new))
                .wrap_err_with(|| format!("probe {url}"))?;
            let rev = immutable_url_of(&resp, url);
            let comparison = if old_rev == Some(rev.as_str()) {
                BranchComparison::verified(CompareStatus::Identical)
            } else {
                BranchComparison::none()
            };
            Ok(CurrentRev { rev, comparison })
        },
    }
}

/// fetch a fixed pin
/// download url bytes, sha256 raw bytes rather than nar, and return the locked
/// node plus the sha256 drift-display rev
/// auto-detect unpack from the url extension when not supplied
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

    // detect from the user-supplied url first, since the immutable url may have
    // lost the extension via a redirect (e.g. github archives -> codeload)
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

/// download a locked tree into dir for inspection
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
            gitlab::fetch_locked_tree_into(host, owner, repo, resolved_rev, dir)
        },
        LockedNode::Fixed { .. } | LockedNode::Indirect { .. } | LockedNode::Path { .. } => {
            bail!("cannot inspect tree for lock type '{}'", node.kind())
        },
    }
}

/// fetch a tree by parsed url into dir
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
    }
}

/// fetch the tree, return (locked node, rev)
pub fn fetch_pin(source: &Source, submodules: bool) -> Result<(LockedNode, String)> {
    fetch_pin_compared(source, submodules, None).map(|fetched| (fetched.node, fetched.rev))
}

pub struct FetchedPin {
    pub node:       LockedNode,
    pub rev:        String,
    pub comparison: BranchComparison,
}

/// fetch the tree and return branch topology against the old revision when
/// available
pub fn fetch_pin_compared(
    source: &Source,
    submodules: bool,
    old_rev: Option<&str>,
) -> Result<FetchedPin> {
    match *source {
        Source::Github {
            ref owner,
            ref repo,
            ref reff,
            rev: ref pinned,
        } => github::fetch_pin_compared(owner, repo, reff.as_deref(), pinned.clone(), old_rev),
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
            Ok(FetchedPin {
                node,
                rev: immutable_url,
                comparison: BranchComparison::none(),
            })
        },
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
) -> Result<FetchedPin> {
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
        Source::Github { .. } | Source::Tarball { .. } => {
            bail!("non-git source cannot be locked from git checkout")
        },
    };

    Ok(FetchedPin {
        node,
        rev,
        comparison: BranchComparison::none(),
    })
}

/// locked url for a tarball response
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

/// extract the immutable url from an http link header per rfc 8288
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

/// get a text resource
pub fn raw(url: &str) -> FetchResult<String> {
    HttpClient::global().raw_text(url)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        current_rev_compared,
        fetch_locked_tree_into,
        git,
        git_pin_from_checkout,
        github::{
            BranchComparison,
            CompareStatus,
        },
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
    fn github_pinned_rev_skips_branch_comparison() {
        let source = "github:o/r?rev=abc123".parse::<Source>().unwrap();
        let mismatched = current_rev_compared(&source, Some("old")).unwrap();
        assert_eq!(mismatched.rev, "abc123");
        assert_eq!(mismatched.comparison, BranchComparison::none());

        let identical = current_rev_compared(&source, Some("abc123")).unwrap();
        assert_eq!(identical.rev, "abc123");
        assert_eq!(
            identical.comparison,
            BranchComparison::verified(CompareStatus::Identical)
        );
    }

    #[test]
    fn link_header_immutable() {
        let immutable = "<https://releases.nixos.org/nixos/abc/nixexprs.tar.xz>; rel=\"immutable\"";
        assert_eq!(
            parse_link_immutable(immutable).as_deref(),
            Some("https://releases.nixos.org/nixos/abc/nixexprs.tar.xz")
        );

        // rel=immutable_link is the historic name used by some nix releases
        let immutable_link = "<https://x/y>; rel=\"immutable_link\"";
        assert_eq!(
            parse_link_immutable(immutable_link).as_deref(),
            Some("https://x/y")
        );

        // link header without immutable rel yields no answer, not the wrong url
        let canonical = "<https://x/y>; rel=\"canonical\"";
        assert!(parse_link_immutable(canonical).is_none());

        // multiple values use the immutable one regardless of position
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
        let fetched = git_pin_from_checkout(
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

        assert_eq!(fetched.rev, "abc123");
        assert_eq!(fetched.comparison, BranchComparison::none());
        assert_eq!(
            fetched.node,
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
        let fetched = git_pin_from_checkout(
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
            fetched.node,
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
