// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    collections::{
        BTreeSet,
        VecDeque,
    },
    env,
    error::Error,
    fs,
    io::{
        self,
        BufRead,
        Read,
    },
    ops::ControlFlow,
    path::Path,
    sync::atomic::AtomicBool,
};

use gix::{
    credentials::{
        helper::Action as CredentialsAction,
        protocol::Result as CredentialsResult,
    },
    objs::{
        self,
        Write as _,
        tree::EntryKind,
    },
    progress::Discard,
    url::Scheme,
};
use gix_pack::bundle::write::Options as PackWriteOptions;
use gix_protocol::{
    fetch::{
        Arguments,
        Response,
    },
    handshake,
    ls_refs::RefPrefixes,
};
use gix_transport::client::blocking_io::{
    ExtendedBufRead,
    HandleProgress,
    Transport,
    connect,
};

use crate::fetch::{
    CommitObject,
    CommitRange,
    CompareStatus,
    FetchError,
    FetchResult,
    auth::record_fetch_warning,
    git_http,
};

const PACK_BYTE_LIMIT: u64 = 64 * 1024 * 1024;
const SPARSE_PACK_BYTE_LIMIT: u64 = 16 * 1024 * 1024;
const SCAN_FILE_BYTE_LIMIT: usize = 4 * 1024 * 1024;
const DEFAULT_DEEPEN_ROUNDS: usize = 3;
const MAX_DEEPEN_ROUNDS: usize = 10;
const DEEPEN_ROUNDS_ENV: &str = "TACK_GIT_DAG_ROUNDS";
const PACK_LIMIT_MARKER: &str = "git DAG pack exceeded";
const RANGE_DEPTHS: [usize; 4] = [1 << 5, 1 << 8, 1 << 11, 1 << 14];

pub(super) fn compare_status(
    url: &str,
    base: &str,
    head: &str,
) -> FetchResult<Option<CompareStatus>> {
    DagGraph::compare(url, parse_object_id(base)?, parse_object_id(head)?)
}

/// the raw commit object, fetched without its tree or history
pub(super) fn commit_object(url: &str, rev: &str) -> FetchResult<CommitObject> {
    let id = parse_object_id(rev)?;
    let mut graph = DagGraph::new(url)?;
    graph.fetch(&[id], 1)?;
    graph
        .commit(id)?
        .map(|(object, _)| object)
        .ok_or_else(|| FetchError::Transport(format!("git commit {id} missing from {url}")))
}

pub(super) fn commit_range(url: &str, base_rev: &str, head_rev: &str) -> FetchResult<CommitRange> {
    let base = parse_object_id(base_rev)?;
    let head = parse_object_id(head_rev)?;
    if base == head {
        return Ok(CommitRange::Commits(Vec::new()));
    }
    for depth in RANGE_DEPTHS {
        let mut graph = DagGraph::new(url)?;
        if let Err(err) = graph.fetch(&[base, head], depth) {
            if is_pack_limit(&err) {
                return Ok(CommitRange::TooLarge);
            }
            return Err(err);
        }
        if let Some(range) = graph.range(base, head)? {
            return Ok(range);
        }
    }
    Ok(CommitRange::TooLarge)
}

pub(super) fn resolve_tip(url: &str, reff: Option<&str>) -> FetchResult<String> {
    let parsed_url = parse_git_url(url)?;
    if let Some(path) = super::local_file_url_path(&parsed_url) {
        return resolve_local_tip(&path, reff);
    }

    let dir = tempfile::tempdir()
        .map_err(|err| FetchError::Transport(format!("create ref probe repo: {err}")))?;
    let repo = gix::init_bare(dir.path())
        .map_err(|err| FetchError::Transport(format!("init ref probe repo: {err}")))?;
    let remote_refs = list_refs(&repo, url, Some(ref_prefixes(reff)))?;
    for candidate in str_ref_candidates(reff) {
        if let Some(object) = remote_refs
            .iter()
            .find_map(|reference| object_for_ref(reference, &candidate))
        {
            return Ok(object.to_string());
        }
    }
    Err(FetchError::NotFound {
        what: reff.map_or_else(
            || "git ref HEAD".to_owned(),
            |target_ref| format!("git ref {target_ref}"),
        ),
    })
}

struct DagGraph {
    dir:        tempfile::TempDir,
    repo:       gix::Repository,
    remote_url: String,
}

impl DagGraph {
    fn compare(
        url: &str,
        base: gix::ObjectId,
        head: gix::ObjectId,
    ) -> FetchResult<Option<CompareStatus>> {
        if base == head {
            return Ok(Some(CompareStatus::Identical));
        }

        for depth in deepen_depths()? {
            let mut graph = Self::new(url)?;
            if let Err(err) = graph.fetch(&[base, head], depth) {
                if is_pack_limit(&err) {
                    return Ok(None);
                }
                return Err(err);
            }
            if let Some(status) = graph.local_status(base, head)? {
                return Ok(Some(status));
            }
        }
        Ok(None)
    }

    fn new(url: &str) -> FetchResult<Self> {
        let dir = tempfile::tempdir()
            .map_err(|err| FetchError::Transport(format!("create dag probe repo: {err}")))?;
        let repo = gix::init_bare(dir.path())
            .map_err(|err| FetchError::Transport(format!("init dag probe repo: {err}")))?;
        Ok(Self {
            dir,
            repo,
            remote_url: url.to_owned(),
        })
    }

    fn fetch(&mut self, wants: &[gix::ObjectId], depth: usize) -> FetchResult<()> {
        filtered_commit_fetch(&self.repo, &self.remote_url, wants, depth)?;
        self.repo = gix::open(self.dir.path())
            .map_err(|err| FetchError::Transport(format!("reopen dag probe repo: {err}")))?;
        Ok(())
    }

    /// [`None`] while the fetched graph is too shallow to tell, since the
    /// anchor's own ancestry is only known as deep as it was fetched
    fn range(&self, base: gix::ObjectId, head: gix::ObjectId) -> FetchResult<Option<CommitRange>> {
        let mut hidden = BTreeSet::new();
        let mut pending = vec![base];
        while let Some(id) = pending.pop() {
            if hidden.insert(id)
                && let Some((_, parents)) = self.commit(id)?
            {
                pending.extend(parents);
            }
        }

        if hidden.contains(&head) {
            return Ok(Some(CommitRange::Ancestor));
        }

        let mut commits = Vec::new();
        let mut seen = BTreeSet::new();
        let mut reached = false;
        let mut walk = vec![head];
        while let Some(id) = walk.pop() {
            if id == base {
                reached = true;
                continue;
            }
            if hidden.contains(&id) || !seen.insert(id) {
                continue;
            }
            let Some((object, parents)) = self.commit(id)? else {
                return Ok(None);
            };
            walk.extend(parents);
            commits.push(object);
        }
        Ok(Some(if reached {
            CommitRange::Commits(commits)
        } else {
            CommitRange::Diverged
        }))
    }

    /// [`None`] when the commit wasn't part of the fetch
    fn commit(&self, id: gix::ObjectId) -> FetchResult<Option<(CommitObject, Vec<gix::ObjectId>)>> {
        let found = self
            .repo
            .try_find_object(id)
            .map_err(|err| FetchError::Transport(format!("read git commit {id}: {err}")))?;
        let Some(object) = found else {
            return Ok(None);
        };
        if object.kind != objs::Kind::Commit {
            return Err(FetchError::Transport(format!(
                "git object {id} is not a commit"
            )));
        }
        let parents = objs::CommitRefIter::from_bytes(&object.data, self.repo.object_hash())
            .parent_ids()
            .collect::<Vec<_>>();
        let commit = CommitObject {
            id:   id.to_string(),
            data: object.data.clone(),
        };
        Ok(Some((commit, parents)))
    }

    fn local_status(
        &self,
        base: gix::ObjectId,
        head: gix::ObjectId,
    ) -> FetchResult<Option<CompareStatus>> {
        let bases = self
            .repo
            .merge_bases_many(base, &[head])
            .map_err(|err| FetchError::Transport(format!("compute dag merge-base: {err}")))?;
        let Some(merge_base) = bases.first().map(|candidate| candidate.detach()) else {
            return Ok(None);
        };
        Ok(Some(if merge_base == base {
            CompareStatus::Ahead
        } else if merge_base == head {
            CompareStatus::Behind
        } else {
            CompareStatus::Diverged
        }))
    }
}

fn list_refs(
    repo: &gix::Repository,
    url: &str,
    prefixes: Option<RefPrefixes>,
) -> FetchResult<Vec<handshake::Ref>> {
    let mut progress = Discard;
    let parsed_url = parse_git_url(url)?;
    let mut authenticate = configured_credentials(repo, parsed_url.clone())?;
    let mut transport = low_level_transport(parsed_url, url)?;
    let mut handshake = gix_protocol::handshake(
        &mut transport,
        gix_transport::Service::UploadPack,
        &mut authenticate,
        Vec::new(),
        &mut progress,
    )
    .map_err(|err| FetchError::Transport(format!("git protocol handshake {url}: {err}")))?;

    if let Some(refs) = handshake.refs.take() {
        return Ok(refs);
    }

    gix_protocol::LsRefsCommand::new(
        prefixes,
        &handshake.capabilities,
        ("agent", Some(Cow::Borrowed("tack"))),
    )
    .invoke_blocking(&mut transport, &mut progress, true)
    .map_err(|err| FetchError::Transport(format!("git ls-refs {url}: {err}")))
}

struct FetchShape<'a> {
    filter:     Option<&'a str>,
    depth:      Option<usize>,
    byte_limit: u64,
}

pub(super) fn fetch_scan_files(
    url: &str,
    rev: &str,
    paths: &[&str],
) -> FetchResult<Vec<Option<String>>> {
    let commit = parse_object_id(rev)?;
    let parsed_url = parse_git_url(url)?;
    if let Some(path) = super::local_file_url_path(&parsed_url) {
        return read_scan_files(&open_local(&path)?, commit, paths);
    }
    // ssh servers usually refuse wants for tree and blob ids, and the ssh child
    // prints that refusal straight to the terminal
    if !matches!(parsed_url.scheme, Scheme::Https | Scheme::Http) {
        return Err(FetchError::Transport(format!(
            "sparse scan needs an http remote, not {url}"
        )));
    }

    let dir = tempfile::tempdir()
        .map_err(|err| FetchError::Transport(format!("create sparse scan repo: {err}")))?;
    let mut repo = gix::init_bare(dir.path())
        .map_err(|err| FetchError::Transport(format!("init sparse scan repo: {err}")))?;
    // github only accepts tree:0, which sends just the wanted object, so the root
    // tree and then the entries under it are fetched one round at a time
    let commit_shape = FetchShape {
        filter:     Some("tree:0"),
        depth:      Some(1),
        byte_limit: SPARSE_PACK_BYTE_LIMIT,
    };
    fetch_pack(&repo, url, parsed_url.clone(), &[commit], &commit_shape)?;
    repo = reopen(dir.path())?;
    let root_shape = FetchShape {
        filter:     Some("tree:0"),
        depth:      None,
        byte_limit: SPARSE_PACK_BYTE_LIMIT,
    };
    fetch_pack(
        &repo,
        url,
        parsed_url.clone(),
        &[root_tree(&repo, commit)?],
        &root_shape,
    )?;
    repo = reopen(dir.path())?;
    let wants = root_wants(&repo, commit, paths)?;
    if !wants.is_empty() {
        let entry_shape = FetchShape {
            filter:     None,
            depth:      None,
            byte_limit: SPARSE_PACK_BYTE_LIMIT,
        };
        fetch_pack(&repo, url, parsed_url, &wants, &entry_shape)?;
        repo = reopen(dir.path())?;
    }
    read_scan_files(&repo, commit, paths)
}

fn reopen(path: &Path) -> FetchResult<gix::Repository> {
    gix::open(path).map_err(|err| FetchError::Transport(format!("reopen sparse scan repo: {err}")))
}

fn open_local(path: &Path) -> FetchResult<gix::Repository> {
    gix::open(path).map_err(|err| {
        FetchError::Transport(format!(
            "open local git repository {}: {err}",
            path.display()
        ))
    })
}

fn root_tree(repo: &gix::Repository, commit: gix::ObjectId) -> FetchResult<gix::ObjectId> {
    let found = repo
        .find_commit(commit)
        .map_err(|err| FetchError::Transport(format!("read git commit {commit}: {err}")))?;
    found
        .tree_id()
        .map(gix::Id::detach)
        .map_err(|err| FetchError::Transport(format!("read tree of git commit {commit}: {err}")))
}

fn tree_entry(
    repo: &gix::Repository,
    tree: gix::ObjectId,
    name: &str,
) -> FetchResult<Option<(EntryKind, gix::ObjectId)>> {
    let object = repo
        .find_object(tree)
        .map_err(|err| FetchError::Transport(format!("read git tree {tree}: {err}")))?;
    let parsed = object
        .try_into_tree()
        .map_err(|_| FetchError::Transport(format!("git object {tree} is not a tree")))?;
    let decoded = parsed
        .decode()
        .map_err(|err| FetchError::Transport(format!("decode git tree {tree}: {err}")))?;
    Ok(decoded
        .entries
        .iter()
        .find(|entry| entry.filename == name.as_bytes())
        .map(|entry| (entry.mode.kind(), entry.oid.to_owned())))
}

fn is_expected_kind(kind: EntryKind, directory: bool) -> bool {
    if directory {
        kind == EntryKind::Tree
    } else {
        matches!(kind, EntryKind::Blob | EntryKind::BlobExecutable)
    }
}

fn root_wants(
    repo: &gix::Repository,
    commit: gix::ObjectId,
    paths: &[&str],
) -> FetchResult<BTreeSet<gix::ObjectId>> {
    let root = root_tree(repo, commit)?;
    let mut wants = BTreeSet::new();
    for path in paths {
        let (top, directory) = path
            .split_once('/')
            .map_or((*path, false), |(dir, _)| (dir, true));
        if let Some((kind, id)) = tree_entry(repo, root, top)?
            && is_expected_kind(kind, directory)
        {
            wants.insert(id);
        }
    }
    Ok(wants)
}

fn read_scan_files(
    repo: &gix::Repository,
    commit: gix::ObjectId,
    paths: &[&str],
) -> FetchResult<Vec<Option<String>>> {
    let root = root_tree(repo, commit)?;
    paths
        .iter()
        .map(|path| read_scan_file(repo, root, path))
        .collect()
}

fn read_scan_file(
    repo: &gix::Repository,
    root: gix::ObjectId,
    path: &str,
) -> FetchResult<Option<String>> {
    let (tree, name) = match path.split_once('/') {
        Some((dir, name)) => {
            match tree_entry(repo, root, dir)? {
                Some((kind, id)) if is_expected_kind(kind, true) => (id, name),
                Some(_) | None => return Ok(None),
            }
        },
        None => (root, path),
    };
    let Some((kind, id)) = tree_entry(repo, tree, name)? else {
        return Ok(None);
    };
    if !is_expected_kind(kind, false) {
        return Ok(None);
    }
    let blob = repo
        .find_object(id)
        .map_err(|err| FetchError::Transport(format!("read git blob {id}: {err}")))?;
    if blob.data.len() > SCAN_FILE_BYTE_LIMIT {
        return Err(FetchError::Transport(format!(
            "git blob {id} for {path} exceeds {SCAN_FILE_BYTE_LIMIT} bytes"
        )));
    }
    Ok(String::from_utf8(blob.data.clone()).ok())
}

fn filtered_commit_fetch(
    repo: &gix::Repository,
    url: &str,
    wants: &[gix::ObjectId],
    depth: usize,
) -> FetchResult<()> {
    let parsed_url = parse_git_url(url)?;
    if let Some(path) = super::local_file_url_path(&parsed_url) {
        return copy_local_commit_graph(&open_local(&path)?, repo, wants, depth);
    }
    let shape = FetchShape {
        filter:     Some("tree:0"),
        depth:      Some(depth),
        byte_limit: PACK_BYTE_LIMIT,
    };
    fetch_pack(repo, url, parsed_url, wants, &shape)
}

fn fetch_pack<'a>(
    repo: &gix::Repository,
    url: &str,
    parsed_url: gix::Url,
    wants: impl IntoIterator<Item = &'a gix::ObjectId>,
    shape: &FetchShape<'_>,
) -> FetchResult<()> {
    let mut progress = Discard;
    let allow_unfiltered = matches!(parsed_url.scheme, Scheme::File);
    let mut authenticate = configured_credentials(repo, parsed_url.clone())?;
    let mut transport = low_level_transport(parsed_url, url)?;
    let handshake = gix_protocol::handshake(
        &mut transport,
        gix_transport::Service::UploadPack,
        &mut authenticate,
        Vec::new(),
        &mut progress,
    )
    .map_err(|err| FetchError::Transport(format!("git protocol handshake {url}: {err}")))?;

    let mut features = gix_protocol::Command::Fetch
        .default_features(handshake.server_protocol_version, &handshake.capabilities);
    features.push(("agent", Some(Cow::Borrowed("tack"))));
    let sideband_all = features.iter().any(|&(name, _)| name == "sideband-all");
    let mut args = Arguments::new(handshake.server_protocol_version, features, false);
    if let Some(filter) = shape.filter {
        if args.can_use_filter() {
            args.filter(filter);
        } else if !allow_unfiltered {
            return Err(FetchError::Transport(format!(
                "git remote does not support filtered fetch: {url}"
            )));
        }
    }
    if let Some(depth) = shape.depth {
        if args.can_use_deepen() {
            args.deepen(depth);
        } else if !allow_unfiltered {
            return Err(FetchError::Transport(format!(
                "git remote does not support shallow fetch: {url}"
            )));
        }
    }
    for want in wants {
        args.want(want);
    }

    let mut reader = args
        .send(&mut transport, true)
        .map_err(|err| FetchError::Transport(format!("git filtered fetch {url}: {err}")))?;
    if sideband_all {
        install_sideband_handler(&mut reader);
    }
    let response =
        Response::from_line_reader(handshake.server_protocol_version, &mut reader, true, false)
            .map_err(|err| {
                FetchError::Transport(format!("read git filtered fetch {url}: {err}"))
            })?;
    if !response.has_pack() {
        return Ok(());
    }
    if !sideband_all {
        install_sideband_handler(&mut reader);
    }
    let pack_dir = repo.path().join("objects").join("pack");
    fs::create_dir_all(&pack_dir)
        .map_err(|err| FetchError::Transport(format!("create pack dir: {err}")))?;
    let interrupt = AtomicBool::new(false);
    let mut capped_reader = CappedBufRead::new(&mut reader, shape.byte_limit);
    let outcome = gix_pack::Bundle::write_to_directory(
        &mut capped_reader,
        Some(&pack_dir),
        &mut progress,
        &interrupt,
        Some(repo.objects.clone()),
        PackWriteOptions {
            object_hash: repo.object_hash(),
            ..Default::default()
        },
    )
    .map_err(|err| {
        FetchError::Transport(format!(
            "write git filtered pack {url}: {}",
            error_chain(&err)
        ))
    })?;
    if let Some(keep_path) = outcome.keep_path {
        let _ = fs::remove_file(keep_path);
    }
    Ok(())
}

fn ref_prefixes(reff: Option<&str>) -> RefPrefixes {
    let mut prefixes = RefPrefixes::new();
    prefixes.extend(
        super::ref_candidates(reff)
            .into_iter()
            .map(|candidate| candidate.unwrap_or_else(|| "HEAD".to_owned()))
            .map(Into::into),
    );
    prefixes
}

fn str_ref_candidates(reff: Option<&str>) -> Vec<String> {
    super::ref_candidates(reff)
        .into_iter()
        .map(|candidate| candidate.unwrap_or_else(|| "HEAD".to_owned()))
        .collect::<Vec<_>>()
}

fn object_for_ref(reference: &handshake::Ref, candidate: &str) -> Option<gix::ObjectId> {
    match *reference {
        handshake::Ref::Peeled {
            ref full_ref_name,
            object,
            ..
        }
        | handshake::Ref::Direct {
            ref full_ref_name,
            object,
        }
        | handshake::Ref::Symbolic {
            ref full_ref_name,
            object,
            ..
        } if full_ref_name.as_slice() == candidate.as_bytes() => Some(object),
        handshake::Ref::Peeled { .. }
        | handshake::Ref::Direct { .. }
        | handshake::Ref::Symbolic { .. }
        | handshake::Ref::Unborn { .. } => None,
    }
}

fn resolve_local_tip(path: &Path, reff: Option<&str>) -> FetchResult<String> {
    let repo = gix::open(path).map_err(|err| {
        FetchError::Transport(format!(
            "open local git repository {}: {err}",
            path.display()
        ))
    })?;
    for candidate in str_ref_candidates(reff) {
        if let Some(object) = local_ref_object(&repo, &candidate)? {
            return Ok(object.to_string());
        }
    }
    Err(FetchError::NotFound {
        what: reff.map_or_else(
            || "git ref HEAD".to_owned(),
            |target_ref| format!("git ref {target_ref}"),
        ),
    })
}

fn local_ref_object(repo: &gix::Repository, candidate: &str) -> FetchResult<Option<gix::ObjectId>> {
    if candidate == "HEAD" {
        return repo
            .head_id()
            .map(|id| Some(id.detach()))
            .map_err(|err| FetchError::Transport(format!("resolve local git ref HEAD: {err}")));
    }
    repo.find_reference(candidate).map_or_else(
        |_| Ok(None),
        |mut reference| {
            reference
                .try_id()
                .map_or_else(
                    || reference.peel_to_id().map(gix::Id::detach),
                    |id| Ok(id.detach()),
                )
                .map(Some)
                .map_err(|err| {
                    FetchError::Transport(format!("resolve local git ref {candidate}: {err}"))
                })
        },
    )
}

fn copy_local_commit_graph(
    source: &gix::Repository,
    dest: &gix::Repository,
    wants: &[gix::ObjectId],
    depth: usize,
) -> FetchResult<()> {
    let mut seen = BTreeSet::new();
    let mut pending = wants
        .iter()
        .copied()
        .map(|want| (want, 0_usize))
        .collect::<VecDeque<_>>();

    while let Some((id, distance)) = pending.pop_front() {
        if !seen.insert(id) {
            continue;
        }

        let object = source
            .find_object(id)
            .map_err(|err| FetchError::Transport(format!("read local git commit {id}: {err}")))?;
        if object.kind != objs::Kind::Commit {
            return Err(FetchError::Transport(format!(
                "local git object {id} is not a commit"
            )));
        }
        dest.objects
            .write_buf(object.kind, &object.data)
            .map_err(|err| FetchError::Transport(format!("write local git commit {id}: {err}")))?;

        if distance + 1 >= depth {
            continue;
        }
        let commit = object.try_into_commit().map_err(|_| {
            FetchError::Transport(format!("local git object {id} changed kind while copying"))
        })?;
        pending.extend(
            commit
                .parent_ids()
                .map(|parent| (parent.detach(), distance + 1)),
        );
    }

    Ok(())
}

#[expect(
    clippy::result_large_err,
    reason = "gix dictates the credential closure's return type"
)]
fn configured_credentials(
    repo: &gix::Repository,
    url: gix::Url,
) -> FetchResult<impl FnMut(CredentialsAction) -> CredentialsResult + 'static> {
    let (mut cascade, _action, prompt_options) = repo
        .config_snapshot()
        .credential_helpers(url)
        .map_err(|err| FetchError::Transport(format!("configure git credentials: {err}")))?;
    Ok(move |action| cascade.invoke(action, prompt_options.clone()))
}

fn low_level_transport(parsed_url: gix::Url, url: &str) -> FetchResult<Box<dyn Transport + Send>> {
    match parsed_url.scheme {
        Scheme::Http | Scheme::Https => Ok(git_http::boxed(parsed_url)),
        Scheme::File | Scheme::Git | Scheme::Ssh | Scheme::Ext(_) => {
            connect::connect(url, connect::Options {
                version: gix_transport::Protocol::V2,
                ..Default::default()
            })
            .map_err(|err| FetchError::Transport(format!("connect git transport {url}: {err}")))
        },
    }
}

fn parse_object_id(rev: &str) -> FetchResult<gix::ObjectId> {
    gix::ObjectId::from_hex(rev.as_bytes())
        .map_err(|err| FetchError::Transport(format!("parse git object id {rev}: {err}")))
}

fn parse_git_url(url: &str) -> FetchResult<gix::Url> {
    gix::Url::try_from(url)
        .map_err(|err| FetchError::Transport(format!("parse git url {url}: {err}")))
}

fn deepen_depths() -> FetchResult<impl Iterator<Item = usize>> {
    configured_rounds().map(|rounds| (0..rounds).map(deepen_depth))
}

fn configured_rounds() -> FetchResult<usize> {
    match env::var(DEEPEN_ROUNDS_ENV) {
        Ok(raw) => parse_rounds(raw.as_str()),
        Err(env::VarError::NotPresent) => Ok(DEFAULT_DEEPEN_ROUNDS),
        Err(env::VarError::NotUnicode(_)) => {
            Err(FetchError::Transport(format!(
                "{DEEPEN_ROUNDS_ENV} must be unicode"
            )))
        },
    }
}

fn parse_rounds(raw_value: &str) -> FetchResult<usize> {
    let trimmed = raw_value.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_DEEPEN_ROUNDS);
    }
    let rounds = trimmed.parse::<usize>().map_err(|err| {
        FetchError::Transport(format!(
            "{DEEPEN_ROUNDS_ENV} must be an integer from 1 to {MAX_DEEPEN_ROUNDS}: {err}"
        ))
    })?;
    if (1..=MAX_DEEPEN_ROUNDS).contains(&rounds) {
        Ok(rounds)
    } else {
        Err(FetchError::Transport(format!(
            "{DEEPEN_ROUNDS_ENV} must be an integer from 1 to {MAX_DEEPEN_ROUNDS}, got {rounds}"
        )))
    }
}

const fn deepen_depth(round: usize) -> usize {
    match round {
        0 => 1,
        1 => 8,
        _ => 1 << (round + 2),
    }
}

/// gix wraps the capped reader's io error in variants whose Display drops the
/// cause
fn error_chain(err: &dyn Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

fn is_pack_limit(err: &FetchError) -> bool {
    match *err {
        FetchError::Transport(ref message) => message.contains(PACK_LIMIT_MARKER),
        FetchError::NotFound { .. }
        | FetchError::Auth { .. }
        | FetchError::RateLimited { .. }
        | FetchError::Decode { .. }
        | FetchError::Github(_)
        | FetchError::Gitlab(_)
        | FetchError::Forge(_) => false,
    }
}

fn install_sideband_handler<'a>(reader: &mut Box<dyn ExtendedBufRead<'a> + Unpin + 'a>) {
    reader.set_progress_handler(Some(Box::new(|is_err: bool, data: &[u8]| {
        if is_err && !data.is_empty() {
            record_fetch_warning(format!(
                "remote: {}",
                String::from_utf8_lossy(data).trim_end()
            ));
        }
        ControlFlow::Continue(())
    }) as HandleProgress<'a>));
}

struct CappedBufRead<'a, R: BufRead + ?Sized> {
    inner:     &'a mut R,
    remaining: u64,
    limit:     u64,
}

impl<'a, R: BufRead + ?Sized> CappedBufRead<'a, R> {
    const fn new(inner: &'a mut R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
            limit,
        }
    }

    fn limit_error(&self) -> io::Error {
        io::Error::other(format!("{PACK_LIMIT_MARKER} {} bytes", self.limit))
    }

    fn cap(remaining: u64, len: usize) -> usize {
        usize::try_from(remaining).map_or(len, |fits| len.min(fits))
    }
}

impl<R: BufRead + ?Sized> Read for CappedBufRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(self.limit_error());
        }
        let max = Self::cap(self.remaining, buf.len());
        let read = self.inner.read(&mut buf[..max])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

impl<R: BufRead + ?Sized> BufRead for CappedBufRead<'_, R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.remaining == 0 {
            return Err(self.limit_error());
        }
        let remaining = self.remaining;
        let available = self.inner.fill_buf()?;
        if available.is_empty() {
            return Ok(available);
        }
        let visible = Self::cap(remaining, available.len());
        Ok(&available[..visible])
    }

    fn consume(&mut self, amount: usize) {
        let consumed = Self::cap(self.remaining, amount);
        self.remaining -= consumed as u64;
        self.inner.consume(consumed);
    }
}

#[cfg(test)]
#[path = "dag_tests.rs"]
mod tests;
