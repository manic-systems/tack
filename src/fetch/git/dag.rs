// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    collections::{
        BTreeSet,
        VecDeque,
    },
    env,
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
    CompareStatus,
    git_http,
    http::{
        FetchError,
        FetchResult,
    },
};

const PACK_BYTE_LIMIT: u64 = 64 * 1024 * 1024;
const DEFAULT_DEEPEN_ROUNDS: usize = 3;
const MAX_DEEPEN_ROUNDS: usize = 10;
const DEEPEN_ROUNDS_ENV: &str = "TACK_GIT_DAG_ROUNDS";
const PACK_LIMIT_MARKER: &str = "git DAG pack exceeded";

pub(super) fn compare_status(
    url: &str,
    base: &str,
    head: &str,
) -> FetchResult<Option<CompareStatus>> {
    DagGraph::compare(url, parse_object_id(base)?, parse_object_id(head)?)
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
    for candidate in ref_candidates(reff) {
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

fn filtered_commit_fetch(
    repo: &gix::Repository,
    url: &str,
    wants: &[gix::ObjectId],
    depth: usize,
) -> FetchResult<()> {
    let mut progress = Discard;
    let parsed_url = parse_git_url(url)?;
    if let Some(path) = super::local_file_url_path(&parsed_url) {
        let source = gix::open(&path).map_err(|err| {
            FetchError::Transport(format!(
                "open local git repository {}: {err}",
                path.display()
            ))
        })?;
        return copy_local_commit_graph(&source, repo, wants, depth);
    }

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
    if args.can_use_filter() {
        args.filter("tree:0");
    } else if !allow_unfiltered {
        return Err(FetchError::Transport(format!(
            "git remote does not support filtered fetch: {url}"
        )));
    }
    if args.can_use_deepen() {
        args.deepen(depth);
    } else if !allow_unfiltered {
        return Err(FetchError::Transport(format!(
            "git remote does not support shallow fetch: {url}"
        )));
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
    let mut capped_reader = CappedBufRead::new(&mut reader, PACK_BYTE_LIMIT);
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
    .map_err(|err| FetchError::Transport(format!("write git filtered pack {url}: {err}")))?;
    if let Some(keep_path) = outcome.keep_path {
        let _ = fs::remove_file(keep_path);
    }
    Ok(())
}

fn ref_prefixes(reff: Option<&str>) -> RefPrefixes {
    let mut prefixes = RefPrefixes::new();
    prefixes.extend(ref_candidates(reff).into_iter().map(Into::into));
    prefixes
}

fn ref_candidates(reff: Option<&str>) -> Vec<String> {
    match reff {
        None => vec!["HEAD".to_owned()],
        Some(target) if target.starts_with("refs/") => vec![target.to_owned()],
        Some(target) => {
            vec![
                format!("refs/heads/{target}"),
                format!("refs/tags/{target}"),
                target.to_owned(),
            ]
        },
    }
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
    for candidate in ref_candidates(reff) {
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
        Ok(raw) => parse_rounds(Some(raw.as_str())),
        Err(env::VarError::NotPresent) => Ok(DEFAULT_DEEPEN_ROUNDS),
        Err(env::VarError::NotUnicode(_)) => {
            Err(FetchError::Transport(format!(
                "{DEEPEN_ROUNDS_ENV} must be unicode"
            )))
        },
    }
}

fn parse_rounds(raw_value: Option<&str>) -> FetchResult<usize> {
    let Some(raw) = raw_value else {
        return Ok(DEFAULT_DEEPEN_ROUNDS);
    };
    let trimmed = raw.trim();
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

fn is_pack_limit(err: &FetchError) -> bool {
    match *err {
        FetchError::Transport(ref message) => message.contains(PACK_LIMIT_MARKER),
        FetchError::NotFound { .. }
        | FetchError::Auth { .. }
        | FetchError::Decode { .. }
        | FetchError::Github(_)
        | FetchError::Gitlab(_) => false,
    }
}

fn install_sideband_handler<'a>(reader: &mut Box<dyn ExtendedBufRead<'a> + Unpin + 'a>) {
    reader.set_progress_handler(Some(Box::new(|is_err: bool, data: &[u8]| {
        if is_err && !data.is_empty() {
            eprintln!("remote: {}", String::from_utf8_lossy(data));
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
mod tests {
    use std::io::{
        Cursor,
        Read as _,
    };

    use super::{
        CappedBufRead,
        compare_status,
        deepen_depth,
        parse_rounds,
        resolve_tip,
    };
    use crate::fetch::{
        CompareStatus,
        git::test_remote::LocalRemote,
    };

    fn linear_remote() -> (LocalRemote, String, String) {
        let mut remote = LocalRemote::new();
        let base = remote.commit("one\n", "one");
        let head = remote.commit("one\ntwo\n", "two");
        (remote, base, head)
    }

    #[test]
    fn resolves_remote_tip_from_refs() {
        let (remote, _, head) = linear_remote();
        remote.tag("v1", &head);
        let url = remote.url();

        assert_eq!(resolve_tip(&url, None).unwrap(), head);
        assert_eq!(resolve_tip(&url, Some("main")).unwrap(), head);
        assert_eq!(resolve_tip(&url, Some("refs/heads/main")).unwrap(), head);
        assert_eq!(resolve_tip(&url, Some("v1")).unwrap(), head);
    }

    #[test]
    fn compares_file_remote_topology() {
        let (remote, base, head) = linear_remote();
        let url = remote.url();

        assert_eq!(
            compare_status(&url, &base, &base).unwrap(),
            Some(CompareStatus::Identical)
        );
        assert_eq!(
            compare_status(&url, &base, &head).unwrap(),
            Some(CompareStatus::Ahead)
        );
        assert_eq!(
            compare_status(&url, &head, &base).unwrap(),
            Some(CompareStatus::Behind)
        );
    }

    #[test]
    fn compares_file_remote_diverged() {
        let mut remote = LocalRemote::new();
        let root = remote.commit("root\n", "root");
        let old = remote.commit("old\n", "old");
        remote.reset_to(&root);
        let new = remote.commit("new\n", "new");
        let url = remote.url();

        assert_eq!(
            compare_status(&url, &old, &new).unwrap(),
            Some(CompareStatus::Diverged)
        );
    }

    #[test]
    fn compare_returns_unverified_when_merge_base_exceeds_probe_depth() {
        let mut remote = LocalRemote::new();
        let root = remote.commit("root\n", "root");
        let mut old = String::new();
        for idx in 0..20_u8 {
            old = remote.commit(&format!("old {idx}\n"), &format!("old {idx}"));
        }
        remote.reset_to(&root);
        let mut new = String::new();
        for idx in 0..20_u8 {
            new = remote.commit(&format!("new {idx}\n"), &format!("new {idx}"));
        }
        let url = remote.url();

        assert_eq!(compare_status(&url, &old, &new).unwrap(), None);
    }

    #[test]
    fn deepen_rounds_are_configured_as_iterations() {
        let depths = (0..5).map(deepen_depth).collect::<Vec<_>>();

        assert_eq!(depths, vec![1, 8, 16, 32, 64]);
        assert_eq!(parse_rounds(None).unwrap(), 3);
        assert_eq!(parse_rounds(Some("")).unwrap(), 3);
        assert_eq!(parse_rounds(Some("4")).unwrap(), 4);
        parse_rounds(Some("0")).unwrap_err();
        parse_rounds(Some("11")).unwrap_err();
        parse_rounds(Some("deep")).unwrap_err();
    }

    #[test]
    fn capped_buf_read_errors_after_limit() {
        let mut input = Cursor::new(b"abcdef".as_slice());
        let mut capped = CappedBufRead::new(&mut input, 3);
        let mut output = Vec::new();

        let err = capped.read_to_end(&mut output).unwrap_err();

        assert_eq!(output, b"abc");
        assert!(err.to_string().contains("git DAG pack exceeded 3 bytes"));
    }
}
