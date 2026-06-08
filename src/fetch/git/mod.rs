// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::BTreeSet,
    fs,
    num::NonZeroU32,
    path::{
        Path,
        PathBuf,
    },
    sync::atomic::AtomicBool,
};

use eyre::{
    ContextCompat as _,
    Result,
    WrapErr as _,
};
use gix::{
    bstr::{
        BStr,
        ByteSlice as _,
    },
    index::write::Options as IndexWriteOptions,
    objs::{
        self,
        Write as _,
        tree::EntryKind,
    },
    progress::Discard,
    refs::transaction::PreviousValue,
    remote::{
        Connection,
        Direction,
        fetch::{
            Shallow,
            Tags,
        },
        ref_map::Options as RefMapOptions,
    },
    submodule::config::Update as SubmoduleUpdate,
    url::Scheme,
};
use gix_transport::client::blocking_io::Transport;
use gix_worktree_state::checkout::Options as CheckoutOptions;

use super::{
    CompareStatus,
    FetchResult,
    git_http,
};
use crate::nar;

mod dag;

#[cfg(test)] mod test_remote;

pub(super) struct PinCheckout {
    pub rev:           String,
    pub nar_hash:      String,
    pub last_modified: i64,
    pub refname:       String,
}

pub(super) fn current_rev(url: &str, reff: Option<&str>, pinned: Option<&str>) -> Result<String> {
    pinned.map_or_else(
        || Ok(dag::resolve_tip(url, reff)?),
        |rev| Ok(rev.to_owned()),
    )
}

pub(super) fn compare_status(
    url: &str,
    base: &str,
    head: &str,
) -> FetchResult<Option<CompareStatus>> {
    dag::compare_status(url, base, head)
}

pub(super) fn fetch_tree_into(
    url: &str,
    reff: Option<&str>,
    rev: Option<&str>,
    submodules: bool,
    dir: &Path,
) -> Result<PathBuf> {
    checkout(url, reff, rev, submodules, dir)?;
    remove_root_git_dir(dir);
    Ok(dir.to_owned())
}

pub(super) fn fetch_tree_rev_into(
    url: &str,
    rev: &str,
    submodules: bool,
    dir: &Path,
) -> Result<PathBuf> {
    checkout_rev(url, rev, submodules, dir)?;
    remove_root_git_dir(dir);
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
    remove_root_git_dir(dir.path());
    let nar_hash = nar::hash_path(dir.path())?;
    Ok(PinCheckout {
        rev,
        nar_hash,
        last_modified,
        refname,
    })
}

fn checkout_rev(url: &str, requested_rev: &str, submodules: bool, into: &Path) -> Result<()> {
    let repo = fetch_pinned(url, None, requested_rev, into)?;
    checkout_existing_commit(&repo, requested_rev)
        .wrap_err_with(|| format!("checkout rev '{requested_rev}' from {url}"))?;
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
    if let Some(rev) = requested_rev {
        let repo = fetch_pinned(url, reff, rev, into)?;
        let (id, time) = checkout_existing_commit(&repo, rev)
            .wrap_err_with(|| format!("checkout rev '{rev}' from {url}"))?;
        if submodules {
            update_submodules(&repo)?;
        }
        let refname = reff.map_or_else(|| "HEAD".to_owned(), str::to_owned);
        Ok((id, time, refname))
    } else {
        let (repo, refname, fetched_ref) = clone_fetch(url, reff, true, into, true)?;
        let commit = fetched_commit(&repo, &fetched_ref)
            .wrap_err_with(|| format!("resolve fetched head from {url}"))?;
        let id = commit.id().detach().to_string();
        let time = commit.time()?.seconds;
        if submodules {
            update_submodules(&repo)?;
        }
        Ok((id, time, refname))
    }
}

fn clone_fetch(
    url: &str,
    reff: Option<&str>,
    shallow: bool,
    into: &Path,
    checkout: bool,
) -> Result<(gix::Repository, String, String)> {
    let candidates = ref_candidates(reff);
    let mut last_err = None;

    for candidate in candidates {
        match clone_fetch_candidate(url, candidate.as_deref(), shallow, into, checkout) {
            Ok((repo, fetched_ref)) => {
                let refname = candidate.unwrap_or_else(|| "HEAD".to_owned());
                return Ok((repo, refname, fetched_ref));
            },
            Err(err) => {
                last_err = Some(err);
                let _ = fs::remove_dir_all(into);
                let _ = fs::create_dir_all(into);
            },
        }
    }

    Err(last_err.unwrap_or_else(|| eyre::eyre!("no ref candidates for {url}")))
}

fn clone_fetch_candidate(
    url: &str,
    reff: Option<&str>,
    shallow: bool,
    into: &Path,
    write_worktree: bool,
) -> Result<(gix::Repository, String)> {
    let repo =
        gix::init(into).wrap_err_with(|| format!("init repository at {}", into.display()))?;
    let (refspecs, fetched_ref) = fetch_refspecs(reff);
    fetch_refspecs_into(&repo, url, &refspecs, shallow)?;

    if write_worktree {
        let commit = fetched_commit(&repo, &fetched_ref)
            .wrap_err_with(|| format!("resolve {fetched_ref}"))?;
        checkout_commit(&repo, &commit).wrap_err_with(|| format!("checkout {url}"))?;
    }

    Ok((repo, fetched_ref))
}

/// fetch the full history of the smallest ref set that contains `pinned`,
/// widening only on a miss
fn fetch_pinned(
    url: &str,
    reff: Option<&str>,
    pinned: &str,
    into: &Path,
) -> Result<gix::Repository> {
    let repo =
        gix::init(into).wrap_err_with(|| format!("init repository at {}", into.display()))?;
    let primary = match reff {
        Some(target) if target.starts_with("refs/") => target.to_owned(),
        Some(target) => format!("refs/heads/{target}"),
        None => "HEAD".to_owned(),
    };
    let rungs = [
        vec![format!("+{primary}:refs/tack/fetched")],
        vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        vec!["+refs/tags/*:refs/tags/*".to_owned()],
    ];

    let mut last_err = None;
    for rung in rungs {
        if let Err(err) = fetch_refspecs_into(&repo, url, &rung, false) {
            last_err = Some(err);
            continue;
        }
        if commit_present(&repo, pinned) {
            return Ok(repo);
        }
    }
    Err(last_err.map_or_else(
        || eyre::eyre!("rev '{pinned}' not reachable from refs on {url}"),
        |err| err.wrap_err(format!("rev '{pinned}' not reachable from refs on {url}")),
    ))
}

fn fetch_refspecs_into(
    repo: &gix::Repository,
    url: &str,
    refspecs: &[String],
    shallow: bool,
) -> Result<()> {
    if let Some(source) = local_file_repo(url)? {
        return fetch_local_refspecs_into(repo, &source, refspecs);
    }

    let mut remote = repo
        .remote_at(url)
        .wrap_err_with(|| format!("prepare remote {url}"))?;
    remote.replace_refspecs(
        refspecs.iter().map(|refspec| BStr::new(refspec.as_str())),
        Direction::Fetch,
    )?;
    remote = remote.with_fetch_tags(Tags::None);
    fetch_into_repo(&remote, url, shallow).wrap_err_with(|| format!("fetch {url}"))
}

fn local_file_repo(url: &str) -> Result<Option<gix::Repository>> {
    let parsed_url = gix::Url::try_from(url)?;
    let Some(path) = local_file_url_path(&parsed_url) else {
        return Ok(None);
    };
    Ok(Some(gix::open(&path).wrap_err_with(|| {
        format!("open local git repository {}", path.display())
    })?))
}

fn local_file_url_path(parsed_url: &gix::Url) -> Option<PathBuf> {
    if parsed_url.scheme != Scheme::File
        || parsed_url
            .host
            .as_deref()
            .is_some_and(|host| host != "localhost")
    {
        return None;
    }
    Some(PathBuf::from(parsed_url.path.to_str_lossy().into_owned()))
}

fn fetch_local_refspecs_into(
    repo: &gix::Repository,
    source: &gix::Repository,
    refspecs: &[String],
) -> Result<()> {
    for refspec in refspecs {
        fetch_local_refspec(repo, source, refspec)?;
    }
    Ok(())
}

fn fetch_local_refspec(
    repo: &gix::Repository,
    source: &gix::Repository,
    refspec: &str,
) -> Result<()> {
    let spec = refspec.strip_prefix('+').unwrap_or(refspec);
    let (source_spec, dest_spec) = spec
        .split_once(':')
        .with_context(|| format!("local fetch refspec '{refspec}' is missing destination"))?;
    if let (Some(source_prefix), Some(dest_prefix)) =
        (source_spec.strip_suffix('*'), dest_spec.strip_suffix('*'))
    {
        return fetch_local_wildcard_refspec(repo, source, source_prefix, dest_prefix);
    }

    let id = local_ref_id(source, source_spec)?;
    let mut seen = BTreeSet::new();
    copy_reachable_object(source, repo, id, &mut seen)?;
    repo.reference(dest_spec, id, PreviousValue::Any, "local fetch")?;
    Ok(())
}

fn fetch_local_wildcard_refspec(
    repo: &gix::Repository,
    source: &gix::Repository,
    source_prefix: &str,
    dest_prefix: &str,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    let reference_platform = source.references()?;
    let references = reference_platform.prefixed(source_prefix)?;
    for reference_result in references {
        let mut reference = reference_result.map_err(|err| eyre::eyre!("{err}"))?;
        let id = reference.try_id().map_or_else(
            || reference.peel_to_id().map(gix::Id::detach),
            |id| Ok(id.detach()),
        )?;
        let full_name = reference.name().as_bstr().to_str_lossy();
        let suffix = full_name
            .strip_prefix(source_prefix)
            .with_context(|| format!("reference {full_name} did not match {source_prefix}"))?
            .to_owned();
        copy_reachable_object(source, repo, id, &mut seen)?;
        repo.reference(
            format!("{dest_prefix}{suffix}"),
            id,
            PreviousValue::Any,
            "local fetch",
        )?;
    }
    Ok(())
}

fn local_ref_id(repo: &gix::Repository, name: &str) -> Result<gix::ObjectId> {
    if name == "HEAD" {
        return Ok(repo.head_id()?.detach());
    }
    let mut reference = repo.find_reference(name)?;
    reference
        .try_id()
        .map_or_else(
            || reference.peel_to_id().map(gix::Id::detach),
            |id| Ok(id.detach()),
        )
        .wrap_err_with(|| format!("resolve local ref {name}"))
}

fn copy_reachable_object(
    source: &gix::Repository,
    dest: &gix::Repository,
    id: gix::ObjectId,
    seen: &mut BTreeSet<gix::ObjectId>,
) -> Result<()> {
    if !seen.insert(id) {
        return Ok(());
    }

    let object = source
        .find_object(id)
        .wrap_err_with(|| format!("read local git object {id}"))?;
    let kind = object.kind;
    dest.objects
        .write_buf(kind, &object.data)
        .map_err(|err| eyre::eyre!("{err}"))?;

    match kind {
        objs::Kind::Commit => {
            let commit = object
                .try_into_commit()
                .map_err(|_| eyre::eyre!("local git object {id} changed kind while copying"))?;
            copy_reachable_object(source, dest, commit.tree_id()?.detach(), seen)?;
            for parent in commit.parent_ids() {
                copy_reachable_object(source, dest, parent.detach(), seen)?;
            }
        },
        objs::Kind::Tree => {
            let tree = object
                .try_into_tree()
                .map_err(|_| eyre::eyre!("local git object {id} changed kind while copying"))?;
            for entry_result in tree.iter() {
                let entry = entry_result?;
                if entry.kind() != EntryKind::Commit {
                    copy_reachable_object(source, dest, entry.id().detach(), seen)?;
                }
            }
        },
        objs::Kind::Tag => {
            let tag = object
                .try_into_tag()
                .map_err(|_| eyre::eyre!("local git object {id} changed kind while copying"))?;
            copy_reachable_object(source, dest, tag.target_id()?.detach(), seen)?;
        },
        objs::Kind::Blob => {},
    }

    Ok(())
}

fn commit_present(repo: &gix::Repository, rev: &str) -> bool {
    gix::ObjectId::from_hex(rev.as_bytes())
        .ok()
        .and_then(|id| repo.find_object(id).ok())
        .and_then(|object| object.peel_to_commit().ok())
        .is_some()
}

fn fetch_into_repo(remote: &gix::Remote<'_>, url: &str, shallow: bool) -> Result<()> {
    let parsed_url = gix::Url::try_from(url)?;
    match parsed_url.scheme {
        Scheme::Http | Scheme::Https => {
            let transport = git_http::connect(parsed_url);
            receive_fetch(remote.to_connection_with_transport(transport), shallow)?;
        },
        Scheme::File | Scheme::Git | Scheme::Ssh | Scheme::Ext(_) => {
            let connection = remote.connect(Direction::Fetch)?;
            receive_fetch(connection, shallow)?;
        },
    }
    Ok(())
}

fn receive_fetch<T>(connection: Connection<'_, '_, T>, shallow: bool) -> Result<()>
where
    T: Transport,
{
    let interrupt = AtomicBool::new(false);
    let mut prepare = connection.prepare_fetch(Discard, RefMapOptions::default())?;
    if shallow {
        prepare = prepare.with_shallow(Shallow::DepthAtRemote(
            NonZeroU32::new(1).expect("constant is non-zero"),
        ));
    }
    prepare.receive(Discard, &interrupt)?;
    Ok(())
}

fn fetched_commit<'repo>(
    repo: &'repo gix::Repository,
    fetched_ref: &str,
) -> Result<gix::Commit<'repo>> {
    let id = repo.find_reference(fetched_ref)?.peel_to_id()?.detach();
    Ok(repo.find_object(id)?.peel_to_commit()?)
}

fn checkout_existing_commit(repo: &gix::Repository, rev: &str) -> Result<(String, i64)> {
    let oid = gix::ObjectId::from_hex(rev.as_bytes())
        .wrap_err_with(|| format!("parse '{rev}' as object id"))?;
    let commit = repo
        .find_object(oid)?
        .peel_to_commit()
        .wrap_err_with(|| format!("peel '{rev}' to commit"))?;
    let id = commit.id().detach().to_string();
    let time = commit.time()?.seconds;
    checkout_commit(repo, &commit)?;
    Ok((id, time))
}

fn checkout_commit(repo: &gix::Repository, commit: &gix::Commit<'_>) -> Result<()> {
    let workdir = repo.workdir().context("gix repository has no worktree")?;
    let tree_id = commit.tree_id()?;
    let mut index = repo.index_from_tree(&tree_id)?;
    let opts = CheckoutOptions {
        destination_is_initially_empty: true,
        ..Default::default()
    };
    gix_worktree_state::checkout(
        &mut index,
        workdir,
        repo.objects.clone().into_arc()?,
        &Discard,
        &Discard,
        &AtomicBool::new(false),
        opts,
    )?;
    index.write(IndexWriteOptions::default())?;
    Ok(())
}

fn update_submodules(repo: &gix::Repository) -> Result<()> {
    let Some(submodules) = repo.submodules()? else {
        return Ok(());
    };

    for submodule in submodules {
        if !submodule.is_active()? {
            continue;
        }
        if matches!(submodule.update()?, Some(SubmoduleUpdate::None)) {
            continue;
        }
        let Some(expected) = submodule.head_id()?.or(submodule.index_id()?) else {
            continue;
        };
        let url = submodule.url()?.to_bstring().to_string();
        let work_dir = submodule.work_dir()?;
        let _ = fs::create_dir_all(&work_dir);
        let sub_repo = fetch_pinned(&url, None, &expected.to_string(), &work_dir)
            .wrap_err_with(|| format!("clone submodule {url}"))?;
        checkout_existing_commit(&sub_repo, &expected.to_string())
            .wrap_err_with(|| format!("checkout submodule {} at {expected}", submodule.name()))?;
    }

    Ok(())
}

fn ref_candidates(reff: Option<&str>) -> Vec<Option<String>> {
    match reff {
        None => vec![None],
        Some(target) if target.starts_with("refs/") => vec![Some(target.to_owned())],
        Some(target) => {
            vec![
                Some(format!("refs/heads/{target}")),
                Some(format!("refs/tags/{target}")),
                Some(target.to_owned()),
            ]
        },
    }
}

/// refspec for a moving ref's tip
fn fetch_refspecs(reff: Option<&str>) -> (Vec<String>, String) {
    reff.map_or_else(
        || {
            let fetched_ref = "refs/remotes/origin/HEAD".to_owned();
            (vec![format!("+HEAD:{fetched_ref}")], fetched_ref)
        },
        |source| {
            let fetched_ref = "refs/tack/fetched".to_owned();
            (vec![format!("+{source}:{fetched_ref}")], fetched_ref)
        },
    )
}

fn remove_root_git_dir(dir: &Path) {
    let _ = fs::remove_dir_all(dir.join(".git"));
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        fetch_tree_into,
        test_remote::LocalRemote,
    };

    #[test]
    fn pinned_rev_reachable_only_off_named_ref_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        let mut remote = LocalRemote::new();
        remote.commit("main\n", "main");
        remote.branch_from_current("refs/heads/feature");
        let pinned = remote.commit("feature\n", "feature");
        let dest = tmp.path().join("out");
        fs::create_dir_all(&dest).unwrap();

        // ref names main, but the pinned rev only lives on feature, so the fetch must
        // widen past the named ref to find it
        fetch_tree_into(&remote.url(), Some("main"), Some(&pinned), false, &dest).unwrap();

        assert_eq!(
            fs::read_to_string(dest.join("file.txt")).unwrap(),
            "feature\n"
        );
    }
}
