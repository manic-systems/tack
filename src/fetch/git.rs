// SPDX-License-Identifier: EUPL-1.2

use std::{
    env,
    fs,
    path::{
        Path,
        PathBuf,
    },
    result::Result as StdResult,
};

use eyre::{
    Result,
    WrapErr as _,
    bail,
};
use git2::{
    Cred,
    CredentialType,
    Direction,
    FetchOptions,
    RemoteCallbacks,
    Repository,
    build::CheckoutBuilder,
};

use super::{
    BranchComparison,
    CompareStatus,
    CurrentRev,
};
use crate::nar;

pub(super) struct PinCheckout {
    pub rev:           String,
    pub nar_hash:      String,
    pub last_modified: i64,
    pub refname:       String,
}

pub(super) fn current_rev_compared(
    url: &str,
    reff: Option<&str>,
    pinned: Option<&str>,
    old_rev: Option<&str>,
) -> Result<CurrentRev> {
    // a pinned rev never moves, skip the network
    if let Some(pinned_rev) = pinned {
        let comparison = if old_rev == Some(pinned_rev) {
            BranchComparison::verified(CompareStatus::Identical)
        } else {
            BranchComparison::none()
        };
        return Ok(CurrentRev {
            rev: pinned_rev.to_owned(),
            comparison,
        });
    }

    let mut remote = git2::Remote::create_detached(url)?;
    let wanted;
    if use_auth_callbacks(url) {
        let conn = remote.connect_auth(Direction::Fetch, Some(callbacks()), None)?;
        let default_ref = branch_str(conn.default_branch());
        let want = resolve_ref(
            default_ref,
            conn.list()?.iter().map(git2::RemoteHead::name),
            reff,
        );
        wanted = want.clone();
        for head in conn.list()? {
            if head.name() == want {
                let head_rev = head.oid().to_string();
                let comparison = if old_rev == Some(head_rev.as_str()) {
                    BranchComparison::verified(CompareStatus::Identical)
                } else {
                    BranchComparison::none()
                };
                return Ok(CurrentRev {
                    rev: head_rev,
                    comparison,
                });
            }
        }
    } else {
        remote.connect(Direction::Fetch)?;
        let default_ref = branch_str(remote.default_branch());
        let want = resolve_ref(
            default_ref,
            remote.list()?.iter().map(git2::RemoteHead::name),
            reff,
        );
        wanted = want.clone();
        for head in remote.list()? {
            if head.name() == want {
                let head_rev = head.oid().to_string();
                let comparison = if old_rev == Some(head_rev.as_str()) {
                    BranchComparison::verified(CompareStatus::Identical)
                } else {
                    BranchComparison::none()
                };
                return Ok(CurrentRev {
                    rev: head_rev,
                    comparison,
                });
            }
        }
        remote.disconnect()?;
    }
    bail!("ref {wanted} not found on {url}")
}

pub(super) fn fetch_tree_into(
    url: &str,
    reff: Option<&str>,
    rev: Option<&str>,
    submodules: bool,
    dir: &Path,
) -> Result<PathBuf> {
    checkout(url, reff, rev, submodules, dir)?;
    let _ = fs::remove_dir_all(dir.join(".git"));
    Ok(dir.to_owned())
}

pub(super) fn fetch_tree_rev_into(
    url: &str,
    rev: &str,
    submodules: bool,
    dir: &Path,
) -> Result<PathBuf> {
    checkout_rev(url, rev, submodules, dir)?;
    let _ = fs::remove_dir_all(dir.join(".git"));
    Ok(dir.to_owned())
}

pub(super) fn fetch_pin_checkout(
    url: &str,
    reff: Option<&str>,
    rev_arg: Option<&str>,
    submodules: bool,
) -> Result<PinCheckout> {
    let dir = tempfile::tempdir()?;
    let (rev, last_modified, refname) = checkout(url, reff, rev_arg, submodules, dir.path())?;
    let _ = fs::remove_dir_all(dir.path().join(".git"));
    let nar_hash = nar::hash_path(dir.path())?;
    Ok(PinCheckout {
        rev,
        nar_hash,
        last_modified,
        refname,
    })
}

fn checkout_rev(url: &str, requested_rev: &str, submodules: bool, into: &Path) -> Result<()> {
    let repo = Repository::init(into)?;
    let mut remote = repo.remote_anonymous(url)?;
    let commit = fetch_pinned(&repo, &mut remote, None, requested_rev, url)?;
    repo.checkout_tree(
        commit.tree()?.as_object(),
        Some(CheckoutBuilder::new().force()),
    )?;
    if submodules {
        update_submodules(&repo)?;
    }
    Ok(())
}

fn checkout(
    url: &str,
    reff: Option<&str>,
    requested_rev: Option<&str>,
    submodules: bool,
    into: &Path,
) -> Result<(String, i64, String)> {
    let repo = Repository::init(into)?;
    let mut remote = repo.remote_anonymous(url)?;

    let (commit, refname) = match requested_rev {
        // a pinned rev can be anywhere in history and git2 0.21 can't want-sha,
        // so widen a full-history fetch only as far as the rev needs
        Some(pinned) => {
            let refname = reff.map_or_else(|| "HEAD".to_owned(), str::to_owned);
            (
                fetch_pinned(&repo, &mut remote, reff, pinned, url)?,
                refname,
            )
        },
        // a moving ref only needs its tip; fetch the refspec directly and read
        // FETCH_HEAD, no preflight ls-remote to resolve it first
        None => fetch_moving(&repo, &mut remote, reff, url)?,
    };

    let rev = commit.id().to_string();
    let time = commit.time().seconds();
    repo.checkout_tree(
        commit.tree()?.as_object(),
        Some(CheckoutBuilder::new().force()),
    )?;
    if submodules {
        update_submodules(&repo)?;
    }
    Ok((rev, time, refname))
}

/// fetch the tip of a moving ref at `depth(1)` without a preflight ls-remote.
/// an unqualified ref might be a branch or a tag, so try `refs/heads/<r>` then
/// `refs/tags/<r>`, peeling `FETCH_HEAD` from whichever lands
fn fetch_moving<'repo>(
    repo: &'repo Repository,
    remote: &mut git2::Remote<'_>,
    reff: Option<&str>,
    url: &str,
) -> Result<(git2::Commit<'repo>, String)> {
    let candidates: Vec<String> = match reff {
        None => vec!["HEAD".to_owned()],
        Some(target) if target.starts_with("refs/") => vec![target.to_owned()],
        Some(target) => {
            vec![
                format!("refs/heads/{target}"),
                format!("refs/tags/{target}"),
                target.to_owned(),
            ]
        },
    };

    for (idx, candidate) in candidates.iter().enumerate() {
        let last = idx + 1 == candidates.len();
        match fetch_refspecs(remote, &[candidate.as_str()], Some(1_i32)) {
            // a fetch can "succeed" yet leave no usable FETCH_HEAD when the ref
            // is absent; treat that like a miss and widen
            Ok(()) => {
                if let Ok(commit) = repo
                    .find_reference("FETCH_HEAD")
                    .and_then(|head| head.peel_to_commit())
                {
                    return Ok((commit, candidate.clone()));
                }
                if last {
                    bail!("ref '{}' not found on {url}", reff.unwrap_or("HEAD"));
                }
            },
            Err(err) if last => {
                return Err(eyre::Report::new(err))
                    .wrap_err_with(|| format!("fetch ref '{candidate}' from {url}"));
            },
            Err(_) => {},
        }
    }
    bail!("ref '{}' not found on {url}", reff.unwrap_or("HEAD"))
}

/// fetch full history of the smallest ref that contains `pinned`, widening the
/// fetch only on a miss: the named ref (or HEAD), then all branches, then tags.
/// objects accumulate across rungs, so each `revparse` sees every fetched ref
fn fetch_pinned<'repo>(
    repo: &'repo Repository,
    remote: &mut git2::Remote<'_>,
    reff: Option<&str>,
    pinned: &str,
    url: &str,
) -> Result<git2::Commit<'repo>> {
    let primary = match reff {
        Some(target) if target.starts_with("refs/") => target.to_owned(),
        Some(target) => format!("refs/heads/{target}"),
        None => "HEAD".to_owned(),
    };
    let rungs: Vec<Vec<&str>> = vec![
        vec![primary.as_str()],
        vec!["+refs/heads/*:refs/remotes/origin/*"],
        vec!["+refs/tags/*:refs/tags/*"],
    ];

    for (idx, rung) in rungs.iter().enumerate() {
        let last = idx + 1 == rungs.len();
        if let Err(err) = fetch_refspecs(remote, rung, None) {
            if last {
                return Err(eyre::Report::new(err))
                    .wrap_err_with(|| format!("fetch refs from {url}"));
            }
            continue;
        }
        if let Ok(commit) = repo
            .revparse_single(pinned)
            .and_then(|obj| obj.peel_to_commit())
        {
            return Ok(commit);
        }
    }
    bail!("rev '{pinned}' not reachable from refs on {url}")
}

fn fetch_refspecs(
    remote: &mut git2::Remote<'_>,
    refspecs: &[&str],
    depth: Option<i32>,
) -> StdResult<(), git2::Error> {
    let mut fo = FetchOptions::new();
    fo.remote_callbacks(callbacks());
    if let Some(limit) = depth {
        fo.depth(limit);
    }
    remote.fetch(refspecs, Some(&mut fo), None)
}

fn update_submodules(repo: &Repository) -> Result<()> {
    for mut sm in repo.submodules()? {
        let mut opts = git2::SubmoduleUpdateOptions::new();
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(callbacks());
        opts.fetch(fo);
        sm.update(true, Some(&mut opts))?;
    }
    Ok(())
}

fn use_auth_callbacks(url: &str) -> bool {
    !url.starts_with("https://") && !url.starts_with("http://")
}

fn branch_str(raw: Result<git2::Buf, git2::Error>) -> Option<String> {
    let buf = raw.ok()?;
    buf.as_str().ok().map(str::to_owned)
}

fn resolve_ref<'a>(
    default_ref: Option<String>,
    heads: impl IntoIterator<Item = &'a str>,
    reff: Option<&str>,
) -> String {
    match reff {
        Some(target) if target.starts_with("refs/") => target.to_owned(),
        Some(target) => {
            let candidates = [
                format!("refs/heads/{target}"),
                format!("refs/tags/{target}"),
                target.to_owned(),
            ];
            heads
                .into_iter()
                .find_map(|name| {
                    candidates
                        .iter()
                        .find(|candidate| name == candidate.as_str())
                })
                .cloned()
                .unwrap_or_else(|| candidates[0].clone())
        },
        None => default_ref.unwrap_or_else(|| "HEAD".to_owned()),
    }
}

fn callbacks() -> RemoteCallbacks<'static> {
    const NAMES: &[&str] = &["id_ed25519", "id_ecdsa", "id_rsa", "id_dsa"];

    let mut cb = RemoteCallbacks::new();
    let mut tried_agent = false;
    let mut key_idx = 0_usize;
    cb.credentials(move |_url, username, allowed| {
        let user = username.unwrap_or("git");
        if allowed.contains(CredentialType::DEFAULT)
            && let Ok(cred) = Cred::default()
        {
            return Ok(cred);
        }
        if allowed.contains(CredentialType::USERNAME) {
            return Cred::username(user);
        }
        if !allowed.contains(CredentialType::SSH_KEY) {
            return Err(git2::Error::from_str("no supported credential type"));
        }
        if !tried_agent {
            tried_agent = true;
            if env::var_os("SSH_AUTH_SOCK").is_some()
                && let Ok(cred) = Cred::ssh_key_from_agent(user)
            {
                return Ok(cred);
            }
        }
        let ssh_dir = env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".ssh"))
            .ok_or_else(|| git2::Error::from_str("ssh: $HOME unset, cannot locate keys"))?;
        while key_idx < NAMES.len() {
            let path = ssh_dir.join(NAMES[key_idx]);
            key_idx += 1;
            if path.is_file() {
                return Cred::ssh_key(user, None, &path, None);
            }
        }
        Err(git2::Error::from_str(
            "ssh: no agent and no usable key under ~/.ssh",
        ))
    });
    cb
}
