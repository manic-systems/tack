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
    FetchedPin,
};
use crate::{
    lock::LockedNode,
    nar,
};

pub(super) fn current_rev_compared(
    url: &str,
    reff: Option<&str>,
    pinned: Option<&str>,
    old_rev: Option<&str>,
) -> Result<CurrentRev> {
    // a pinned rev never moves, so report it without touching the network
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

    let cb = callbacks();
    let mut remote = git2::Remote::create_detached(url)?;
    let conn = remote.connect_auth(Direction::Fetch, Some(cb), None)?;
    let want = full_ref(reff, || branch_str(conn.default_branch()));
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
    bail!("ref {want} not found on {url}")
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

pub(super) fn fetch_pin_compared(
    url: &str,
    reff: Option<&str>,
    rev_arg: Option<&str>,
    submodules: bool,
) -> Result<FetchedPin> {
    let dir = tempfile::tempdir()?;
    let (rev, last_modified, refname) = checkout(url, reff, rev_arg, submodules, dir.path())?;
    let _ = fs::remove_dir_all(dir.path().join(".git"));
    let nar_hash = nar::hash_path(dir.path())?;
    let node = LockedNode::new_git(
        url,
        refname,
        rev.clone(),
        nar_hash,
        last_modified,
        submodules,
    );
    Ok(FetchedPin {
        node,
        rev,
        comparison: BranchComparison::none(),
    })
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

    let refname = {
        let conn = remote.connect_auth(Direction::Fetch, Some(callbacks()), None)?;
        full_ref(reff, || branch_str(conn.default_branch()))
    };

    let mut fo = FetchOptions::new();
    fo.remote_callbacks(callbacks());
    // a specific rev can be anywhere in history, so fetch the ref in full for a
    // moving ref we only need the tip
    if requested_rev.is_none() {
        fo.depth(1);
    }
    remote
        .fetch(&[&refname], Some(&mut fo), None)
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

fn branch_str(raw: Result<git2::Buf, git2::Error>) -> Option<String> {
    let buf = raw.ok()?;
    buf.as_str().ok().map(str::to_owned)
}

fn full_ref(reff: Option<&str>, default: impl FnOnce() -> Option<String>) -> String {
    match reff {
        Some(target) if target.starts_with("refs/") => target.to_owned(),
        Some(target) => format!("refs/heads/{target}"),
        None => default().unwrap_or_else(|| "HEAD".to_owned()),
    }
}

fn callbacks() -> RemoteCallbacks<'static> {
    const NAMES: &[&str] = &["id_ed25519", "id_ecdsa", "id_rsa", "id_dsa"];

    let mut cb = RemoteCallbacks::new();
    let mut tried_agent = false;
    let mut key_idx = 0_usize;
    cb.credentials(move |_url, username, allowed| {
        let user = username.unwrap_or("git");
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
