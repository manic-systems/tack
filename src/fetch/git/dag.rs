// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    collections::BTreeSet,
    fs,
    io::{
        self,
        BufRead,
        Read,
    },
    sync::atomic::AtomicBool,
};

use gix::{
    progress::Discard,
    url::Scheme,
};
use gix_transport::client::blocking_io::{
    HandleProgress,
    Transport,
};

use crate::fetch::{
    git_http,
    github::CompareStatus,
    http::{
        FetchError,
        FetchResult,
    },
};

const PACK_BYTE_LIMIT: u64 = 64 * 1024 * 1024;

pub(super) fn compare_status(
    url: &str,
    base: &str,
    head: &str,
) -> FetchResult<Option<CompareStatus>> {
    DagGraph::compare(url, parse_object_id(base)?, parse_object_id(head)?)
}

pub(super) fn resolve_tip(url: &str, reff: Option<&str>) -> FetchResult<String> {
    let dir = tempfile::tempdir()
        .map_err(|err| FetchError::Transport(format!("create ref probe repo: {err}")))?;
    let repo = gix::init_bare(dir.path())
        .map_err(|err| FetchError::Transport(format!("init ref probe repo: {err}")))?;
    let refs = list_refs(&repo, url, Some(ref_prefixes(reff)))?;
    for candidate in ref_candidates(reff) {
        if let Some(object) = refs
            .iter()
            .find_map(|reference| object_for_ref(reference, &candidate))
        {
            return Ok(object.to_string());
        }
    }
    Err(FetchError::NotFound {
        what: reff.map_or_else(
            || "git ref HEAD".to_owned(),
            |reff| format!("git ref {reff}"),
        ),
    })
}

struct DagGraph {
    dir:        tempfile::TempDir,
    repo:       gix::Repository,
    remote_url: String,
    shallows:   BTreeSet<gix::ObjectId>,
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

        let mut graph = Self::new(url)?;
        graph.fetch(base, &[], true)?;
        graph.fetch(head, &[base], false)?;
        if let Some(status) = graph.local_status(base, head)? {
            return Ok(Some(status));
        }

        graph.fetch(base, &[head], false)?;
        Ok(Some(
            graph
                .local_status(base, head)?
                .unwrap_or(CompareStatus::Diverged),
        ))
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
            shallows: BTreeSet::new(),
        })
    }

    fn fetch(
        &mut self,
        want: gix::ObjectId,
        haves: &[gix::ObjectId],
        shallow: bool,
    ) -> FetchResult<()> {
        filtered_commit_fetch(
            &self.repo,
            &self.remote_url,
            want,
            haves,
            shallow,
            &mut self.shallows,
        )?;
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
        let Some(merge_base) = bases.first().map(|base| base.detach()) else {
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
    prefixes: Option<gix_protocol::ls_refs::RefPrefixes>,
) -> FetchResult<Vec<gix_protocol::handshake::Ref>> {
    let mut progress = Discard;
    let parsed_url = gix::Url::try_from(url)
        .map_err(|err| FetchError::Transport(format!("parse git url {url}: {err}")))?;
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
    want: gix::ObjectId,
    haves: &[gix::ObjectId],
    shallow: bool,
    shallows: &mut BTreeSet<gix::ObjectId>,
) -> FetchResult<()> {
    let allow_unfiltered = gix::Url::try_from(url)
        .ok()
        .is_some_and(|parsed| matches!(parsed.scheme, Scheme::File));
    let mut progress = Discard;
    let parsed_url = gix::Url::try_from(url)
        .map_err(|err| FetchError::Transport(format!("parse git url {url}: {err}")))?;
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
    let sideband_all = features.iter().any(|(name, _)| *name == "sideband-all");
    let mut args =
        gix_protocol::fetch::Arguments::new(handshake.server_protocol_version, features, false);
    if !args.can_use_filter() {
        if !allow_unfiltered {
            return Err(FetchError::Transport(format!(
                "git remote does not support filtered fetch: {url}"
            )));
        }
    } else {
        args.filter("tree:0");
    }
    if !shallows.is_empty() && !args.can_use_shallow() {
        if !allow_unfiltered {
            return Err(FetchError::Transport(format!(
                "git remote does not support shallow boundary negotiation: {url}"
            )));
        }
    } else {
        for shallow_boundary in shallows.iter().copied() {
            args.shallow(shallow_boundary);
        }
    }
    if shallow && !args.can_use_deepen() {
        if !allow_unfiltered {
            return Err(FetchError::Transport(format!(
                "git remote does not support shallow fetch: {url}"
            )));
        }
    } else if shallow {
        args.deepen(1);
    }
    args.want(want);
    for have in haves {
        args.have(have);
    }

    let mut reader = args
        .send(&mut transport, true)
        .map_err(|err| FetchError::Transport(format!("git filtered fetch {url}: {err}")))?;
    if sideband_all {
        install_sideband_handler(&mut reader);
    }
    let response = gix_protocol::fetch::Response::from_line_reader(
        handshake.server_protocol_version,
        &mut reader,
        true,
        false,
    )
    .map_err(|err| FetchError::Transport(format!("read git filtered fetch {url}: {err}")))?;
    apply_shallow_updates(shallows, response.shallow_updates());
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
        gix_pack::bundle::write::Options {
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

fn ref_prefixes(reff: Option<&str>) -> gix_protocol::ls_refs::RefPrefixes {
    let mut prefixes = gix_protocol::ls_refs::RefPrefixes::new();
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

fn object_for_ref(
    reference: &gix_protocol::handshake::Ref,
    candidate: &str,
) -> Option<gix::ObjectId> {
    match reference {
        gix_protocol::handshake::Ref::Peeled {
            full_ref_name,
            object,
            ..
        }
        | gix_protocol::handshake::Ref::Direct {
            full_ref_name,
            object,
        }
        | gix_protocol::handshake::Ref::Symbolic {
            full_ref_name,
            object,
            ..
        } if full_ref_name.as_slice() == candidate.as_bytes() => Some(*object),
        _ => None,
    }
}

fn apply_shallow_updates(
    shallows: &mut BTreeSet<gix::ObjectId>,
    updates: &[gix_protocol::fetch::response::ShallowUpdate],
) {
    for update in updates {
        match update {
            gix_protocol::fetch::response::ShallowUpdate::Shallow(id) => {
                shallows.insert(*id);
            },
            gix_protocol::fetch::response::ShallowUpdate::Unshallow(id) => {
                shallows.remove(id);
            },
        }
    }
}

fn configured_credentials(
    repo: &gix::Repository,
    url: gix::Url,
) -> FetchResult<
    impl FnMut(gix::credentials::helper::Action) -> gix::credentials::protocol::Result + 'static,
> {
    let (mut cascade, _action, prompt_options) = repo
        .config_snapshot()
        .credential_helpers(url)
        .map_err(|err| FetchError::Transport(format!("configure git credentials: {err}")))?;
    Ok(move |action| cascade.invoke(action, prompt_options.clone()))
}

fn low_level_transport(parsed_url: gix::Url, url: &str) -> FetchResult<Box<dyn Transport + Send>> {
    match parsed_url.scheme {
        Scheme::Http | Scheme::Https => Ok(git_http::boxed(parsed_url)),
        _ => {
            gix_transport::client::blocking_io::connect::connect(
                url,
                gix_transport::client::blocking_io::connect::Options {
                    version: gix_transport::Protocol::V2,
                    ..Default::default()
                },
            )
            .map_err(|err| FetchError::Transport(format!("connect git transport {url}: {err}")))
        },
    }
}

fn parse_object_id(rev: &str) -> FetchResult<gix::ObjectId> {
    gix::ObjectId::from_hex(rev.as_bytes())
        .map_err(|err| FetchError::Transport(format!("parse git object id {rev}: {err}")))
}

fn install_sideband_handler<'a>(
    reader: &mut Box<dyn gix_transport::client::blocking_io::ExtendedBufRead<'a> + Unpin + 'a>,
) {
    reader.set_progress_handler(Some(Box::new(|is_err: bool, data: &[u8]| {
        if is_err && !data.is_empty() {
            eprintln!("remote: {}", String::from_utf8_lossy(data));
        }
        std::ops::ControlFlow::Continue(())
    }) as HandleProgress<'a>));
}

struct CappedBufRead<'a, R: BufRead + ?Sized> {
    inner:     &'a mut R,
    remaining: u64,
    limit:     u64,
}

impl<'a, R: BufRead + ?Sized> CappedBufRead<'a, R> {
    fn new(inner: &'a mut R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
            limit,
        }
    }

    fn limit_error(&self) -> io::Error {
        io::Error::other(format!("git DAG pack exceeded {} bytes", self.limit))
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
        let max = buf.len().min(self.remaining as usize);
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
        let available = self.inner.fill_buf()?;
        if available.is_empty() {
            return Ok(available);
        }
        let visible = available.len().min(self.remaining as usize);
        Ok(&available[..visible])
    }

    fn consume(&mut self, amt: usize) {
        let consumed = amt.min(self.remaining as usize);
        self.remaining -= consumed as u64;
        self.inner.consume(consumed);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{
            Cursor,
            Read as _,
        },
        path::{
            Path,
            PathBuf,
        },
        process::Command,
    };

    use super::{
        CappedBufRead,
        compare_status,
        resolve_tip,
    };
    use crate::fetch::github::CompareStatus;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|err| panic!("run git {args:?}: {err}"));
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git output is utf-8")
            .trim()
            .to_owned()
    }

    struct LocalRemote {
        _tmp:   tempfile::TempDir,
        work:   PathBuf,
        remote: PathBuf,
    }

    impl LocalRemote {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let work = tmp.path().join("work");
            let remote = tmp.path().join("remote.git");

            git(tmp.path(), &["init", work.to_str().unwrap()]);
            git(&work, &["config", "user.email", "tack@example.invalid"]);
            git(&work, &["config", "user.name", "tack"]);
            git(tmp.path(), &["init", "--bare", remote.to_str().unwrap()]);
            git(&work, &[
                "remote",
                "add",
                "origin",
                remote.to_str().unwrap(),
            ]);
            Self {
                _tmp: tmp,
                work,
                remote,
            }
        }

        fn commit(&self, body: &str, message: &str) -> String {
            std::fs::write(self.work.join("file.txt"), body).unwrap();
            git(&self.work, &["add", "file.txt"]);
            git(&self.work, &["commit", "-m", message]);
            git(&self.work, &["rev-parse", "HEAD"])
        }

        fn push(&self, force: bool) {
            let mut args = vec!["push"];
            if force {
                args.push("--force");
            }
            args.extend(["-u", "origin", "main"]);
            git(&self.work, &args);
        }

        fn set_head(&self, branch: &str) {
            git(&self.remote, &["symbolic-ref", "HEAD", branch]);
        }

        fn url(&self) -> String {
            format!("file://{}", self.remote.display())
        }
    }

    fn linear_remote() -> (LocalRemote, String, String) {
        let remote = LocalRemote::new();
        let base = remote.commit("one\n", "one");
        git(&remote.work, &["branch", "-M", "main"]);
        remote.push(false);
        remote.set_head("refs/heads/main");
        let head = remote.commit("one\ntwo\n", "two");
        remote.push(false);
        (remote, base, head)
    }

    #[test]
    fn resolves_remote_tip_from_refs() {
        let (remote, _, head) = linear_remote();
        git(&remote.work, &["tag", "-a", "v1", "-m", "v1", "HEAD"]);
        git(&remote.work, &["push", "origin", "v1"]);
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
        let remote = LocalRemote::new();
        remote.commit("root\n", "root");
        git(&remote.work, &["branch", "-M", "main"]);
        let old = remote.commit("old\n", "old");
        remote.push(false);
        git(&remote.work, &["reset", "--hard", "HEAD~1"]);
        let new = remote.commit("new\n", "new");
        remote.push(true);
        let url = remote.url();

        assert_eq!(
            compare_status(&url, &old, &new).unwrap(),
            Some(CompareStatus::Diverged)
        );
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
