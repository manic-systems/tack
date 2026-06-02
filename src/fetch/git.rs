// SPDX-License-Identifier: EUPL-1.2

use std::{
    env,
    fs,
    path::{
        Path,
        PathBuf,
    },
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
    let mut fo = fetch_options(url);
    remote
        .fetch(
            &[
                "+refs/heads/*:refs/remotes/origin/*",
                "+refs/tags/*:refs/tags/*",
            ],
            Some(&mut fo),
            None,
        )
        .wrap_err_with(|| format!("fetch refs from {url}"))?;

    let commit = repo
        .revparse_single(requested_rev)
        .wrap_err_with(|| format!("rev '{requested_rev}' not reachable from refs on {url}"))?
        .peel_to_commit()
        .wrap_err_with(|| format!("'{requested_rev}' is not a commit"))?;

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

    let (refname, fetch_all_refs) = {
        if use_auth_callbacks(url) {
            let conn = remote.connect_auth(Direction::Fetch, Some(callbacks()), None)?;
            let default_ref = branch_str(conn.default_branch());
            let refname = if requested_rev.is_some() && reff.is_none() {
                default_ref.unwrap_or_else(|| "HEAD".to_owned())
            } else {
                resolve_ref(
                    default_ref,
                    conn.list()?.iter().map(git2::RemoteHead::name),
                    reff,
                )
            };
            (refname, requested_rev.is_some() && reff.is_none())
        } else {
            remote.connect(Direction::Fetch)?;
            let default_ref = branch_str(remote.default_branch());
            let refname = if requested_rev.is_some() && reff.is_none() {
                default_ref.unwrap_or_else(|| "HEAD".to_owned())
            } else {
                resolve_ref(
                    default_ref,
                    remote.list()?.iter().map(git2::RemoteHead::name),
                    reff,
                )
            };
            remote.disconnect()?;
            (refname, requested_rev.is_some() && reff.is_none())
        }
    };

    let mut fo = fetch_options(url);
    // a specific rev can be anywhere in history, so fetch the ref in full;
    // for a moving ref we only need the tip
    if requested_rev.is_none() {
        fo.depth(1);
    }
    let fetch_refspecs = if fetch_all_refs {
        vec![
            "+refs/heads/*:refs/remotes/origin/*".to_owned(),
            "+refs/tags/*:refs/tags/*".to_owned(),
        ]
    } else {
        vec![refname.clone()]
    };
    let fetch_refs = fetch_refspecs
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    remote
        .fetch(&fetch_refs, Some(&mut fo), None)
        .wrap_err_with(|| format!("fetch {refname} from {url}"))?;

    let commit = match requested_rev {
        Some(pinned) => {
            repo.revparse_single(pinned)
                .wrap_err_with(|| format!("rev '{pinned}' not reachable from {refname} on {url}"))?
                .peel_to_commit()
                .wrap_err_with(|| format!("'{pinned}' is not a commit"))?
        },
        None => repo.find_reference("FETCH_HEAD")?.peel_to_commit()?,
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

fn fetch_options(url: &str) -> FetchOptions<'static> {
    let mut fo = FetchOptions::new();
    if use_auth_callbacks(url) {
        fo.remote_callbacks(callbacks());
    }
    fo
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
