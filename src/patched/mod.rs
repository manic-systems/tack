// SPDX-License-Identifier: EUPL-1.2

mod apply;
pub mod source;
pub mod store;

use std::{
    fs,
    io::ErrorKind,
    path::{
        Path,
        PathBuf,
    },
    time::UNIX_EPOCH,
};

use apply::SourceTree;
use data_encoding::HEXLOWER;
use hmac_sha256::Hash as Sha256;
use misstep::{
    Result,
    ResultExt as _,
    bail,
};
use source::PatchSource;
use store::StorePath;

use crate::{
    error::user_bail,
    fetch::{
        self,
        FetchedTree,
    },
    history::HistoryStore,
    lock::{
        LockFile,
        LockedNode,
        PatchDigest,
        PatchedTree,
    },
    nar,
    project::{
        Project,
        write_atomic,
    },
    report::PullPatch,
};

struct PatchFile {
    source:     String,
    url:        Option<String>,
    file:       String,
    bytes:      Vec<u8>,
    head:       Option<String>,
    downloaded: bool,
}

impl PatchFile {
    fn read(project: &Project, source: &str, file: &str, head: Option<String>) -> Result<Self> {
        let path = project.dir().join(file);
        match fs::read(&path) {
            Ok(bytes) => {
                Ok(Self {
                    source: source.to_owned(),
                    url: None,
                    file: file.to_owned(),
                    bytes,
                    head,
                    downloaded: false,
                })
            },
            Err(err) => user_bail!("read patch {}: {err}", path.display()),
        }
    }
}

impl From<&PatchFile> for PatchDigest {
    fn from(patch: &PatchFile) -> Self {
        Self {
            source: patch.source.clone(),
            url:    patch.url.clone(),
            file:   patch.file.clone(),
            sha256: HEXLOWER.encode(&Sha256::hash(&patch.bytes)),
            head:   patch.head.clone(),
        }
    }
}

/// what a caller lets [`PatchedPin::settle`] do
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// reuse every vendored patch the lock knows and rebuild when anything
    /// changed, so a force-pushed pull request can't slip in
    Update,
    /// download remote patches again
    Refresh,
    /// rebuild exactly the locked tree and refuse anything else
    Restore,
}

pub enum Settled {
    Current,
    Restored,
    Rebuilt(PatchedTree),
    /// the same patches on the same node built a tree the lock hashes
    /// differently, as when patch application itself changed
    Rehashed(PatchedTree),
    Unpatched,
}

impl Settled {
    pub fn record_into(self, lock: &mut LockFile, pin: &str) -> bool {
        match self {
            Self::Current | Self::Restored => false,
            Self::Rebuilt(tree) | Self::Rehashed(tree) => lock.set_patched(pin, Some(tree)),
            Self::Unpatched => lock.set_patched(pin, None),
        }
    }
}

pub struct PatchedPin<'a> {
    project:  &'a Project,
    name:     &'a str,
    node:     &'a LockedNode,
    upstream: Option<FetchedTree>,
}

impl<'a> PatchedPin<'a> {
    pub const fn new(project: &'a Project, name: &'a str, node: &'a LockedNode) -> Self {
        Self {
            project,
            name,
            node,
            upstream: None,
        }
    }

    /// `upstream` must be the tree `node` was hashed from
    pub fn with_upstream(self, upstream: Option<FetchedTree>) -> Self {
        Self { upstream, ..self }
    }

    /// brings the patched tree of `node` in line with `sources`, where `locked`
    /// is the tree the lock holds for this pin and `moved` says it was built on
    /// another node, and vendors downloads only once they apply
    pub fn settle(
        mut self,
        sources: &[PatchSource],
        locked: Option<&PatchedTree>,
        moved: bool,
        mode: Mode,
    ) -> Result<Settled> {
        let name = self.name;
        if sources.is_empty() {
            unroot(self.project, name)?;
            return Ok(Settled::Unpatched);
        }
        if matches!(
            *self.node,
            LockedNode::Path { .. } | LockedNode::Indirect { .. }
        ) {
            user_bail!("patches need a github, gitlab, git or tarball pin");
        }
        let patches = self.collect(sources, locked, mode)?;
        let digests = patches.iter().map(PatchDigest::from).collect::<Vec<_>>();
        let current = locked.filter(|tree| !moved && tree.patches == digests);
        // a hand-edited or badly merged lock can name a hash the path doesn't have
        let recorded = current
            .filter(|tree| tree.path.exists())
            .map(|tree| tree.path.recorded_nar_hash());
        let settled = match (current, mode) {
            (Some(tree), _)
                if tree.path.exists()
                    && recorded.flatten().is_none_or(|hash| hash == tree.nar_hash) =>
            {
                tree.path.root_at(&gcroot(self.project, name))?;
                Settled::Current
            },
            (Some(tree), Mode::Restore) if tree.path.exists() => {
                user_bail!(
                    "the lock's narHash {} is not the patched tree's, run `tack update {name}`",
                    tree.nar_hash
                )
            },
            (Some(tree), Mode::Restore) => {
                self.materialize(&patches, digests, Some(&tree.nar_hash))?;
                Settled::Restored
            },
            (Some(tree), Mode::Update | Mode::Refresh) => {
                let rebuilt = self.materialize(&patches, digests, None)?;
                if rebuilt.nar_hash == tree.nar_hash {
                    Settled::Restored
                } else {
                    Settled::Rehashed(rebuilt)
                }
            },
            (None, Mode::Restore) => {
                user_bail!("the patches no longer match the lock, run `tack update {name}`")
            },
            (None, Mode::Update | Mode::Refresh) => {
                Settled::Rebuilt(self.materialize(&patches, digests, None)?)
            },
        };
        // a vendored file whose source now vendors elsewhere, as when a shorturl
        // alias was repointed, is left over
        let orphans = locked
            .into_iter()
            .flat_map(|tree| &tree.patches)
            .filter(|digest| digest.url.is_some())
            .filter(|digest| patches.iter().all(|patch| patch.file != digest.file));
        for digest in orphans {
            remove_stale(&self.project.dir().join(&digest.file))?;
        }
        for patch in &patches {
            if patch.downloaded {
                let target = self.project.dir().join(&patch.file);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                write_atomic(&target, &patch.bytes)?;
            }
            remove_stale(&self.project.dir().join(format!("{}.rej", patch.file)))?;
        }
        Ok(settled)
    }

    fn collect(
        &self,
        sources: &[PatchSource],
        locked: Option<&PatchedTree>,
        mode: Mode,
    ) -> Result<Vec<PatchFile>> {
        let name = self.name;
        let mut patches = Vec::<PatchFile>::with_capacity(sources.len());
        for entry in sources {
            let source = entry.to_string();
            if patches.iter().any(|patch| patch.source == source) {
                user_bail!("patch {source} is listed twice");
            }
            let patch = match *entry {
                PatchSource::Local(ref file) => PatchFile::read(self.project, &source, file, None)?,
                PatchSource::Remote(ref remote) => {
                    let file = remote.vendored_file(name);
                    if patches.iter().any(|patch| patch.file == file) {
                        user_bail!(
                            "{source} vendors to {file} like another patch does, which looks like \
                             the same patch"
                        );
                    }
                    // a vendored file the lock doesn't know is left over from an undo
                    let url = remote.url();
                    let known = locked
                        .into_iter()
                        .flat_map(|tree| &tree.patches)
                        .find(|digest| {
                            digest.source == source && digest.url.as_deref() == Some(url)
                        });
                    if mode != Mode::Refresh
                        && let Some(digest) = known
                        && self.project.dir().join(&file).exists()
                    {
                        PatchFile {
                            url: Some(url.to_owned()),
                            ..PatchFile::read(self.project, &source, &file, digest.head.clone())?
                        }
                    } else {
                        let (bytes, head) = remote.download()?;
                        if bytes.trim_ascii().is_empty() {
                            user_bail!("{source}: the patch is empty");
                        }
                        PatchFile {
                            source,
                            url: Some(url.to_owned()),
                            file,
                            bytes,
                            head,
                            downloaded: true,
                        }
                    }
                },
            };
            patches.push(patch);
        }
        Ok(patches)
    }

    fn materialize(
        &mut self,
        patches: &[PatchFile],
        digests: Vec<PatchDigest>,
        locked: Option<&str>,
    ) -> Result<PatchedTree> {
        let name = self.name;
        let scratch = tempfile::tempdir()?;
        // nix-store names the path after the directory, and fetchTree only reuses
        // a tree named `source` instead of copying it again
        let tree = scratch.path().join("source");
        if let Some(upstream) = self.upstream.take() {
            fs::rename(upstream.root(), &tree)?;
        } else {
            let upstream_dir = scratch.path().join("upstream");
            fs::create_dir_all(&upstream_dir)?;
            let upstream = fetch::fetch_locked_tree_into(self.node, &upstream_dir)
                .context("fetch the locked source")?;
            if let Some(expected) = self.node.hash() {
                let actual = nar::hash_path(&upstream)?;
                if actual != expected {
                    bail!("fetched source hashes to {actual}, but the lock says {expected}");
                }
            }
            fs::rename(&upstream, &tree)?;
        }

        let newest = newest_mtime(&tree);
        let source_tree = SourceTree::new(&tree)?;
        for patch in patches {
            if let Err(err) = source_tree.apply(&patch.bytes) {
                let source = &patch.source;
                if source_tree.already_applied(&patch.bytes) {
                    user_bail!(
                        "{source} is already applied, upstream has it or an earlier patch does"
                    );
                }
                let rejects = source_tree.rejects(&patch.bytes);
                if !rejects.is_empty() {
                    let reject_file = format!("{}.rej", patch.file);
                    let target = self.project.dir().join(&reject_file);
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    write_atomic(&target, &rejects)?;
                    return Err(err).with_context(|| {
                        format!("apply {source}, the hunks that failed are in {reject_file}")
                    });
                }
                return Err(err).with_context(|| format!("apply {source}"));
            }
        }

        let nar_hash = nar::hash_path(&tree)?;
        if let Some(expected) = locked
            && nar_hash != expected
        {
            bail!("rebuilt patched tree hashes to {nar_hash}, but the lock says {expected}");
        }
        let path = StorePath::add(&tree)?;
        path.root_at(&gcroot(self.project, name))?;
        Ok(PatchedTree {
            path,
            nar_hash,
            last_modified: self
                .node
                .last_modified()
                .filter(|&modified| modified > 0)
                .or(newest),
            patches: digests,
        })
    }
}

/// asks each forge what became of the pull requests among `sources`, with
/// `upstream` the newest rev of the pin `node` locks, and names each source
/// it couldn't ask with the reason
pub fn pull_patches(
    sources: &[PatchSource],
    tree: &PatchedTree,
    node: &LockedNode,
    upstream: Option<&str>,
) -> (Vec<PullPatch>, Vec<String>) {
    let mut pulls = Vec::new();
    let mut unchecked = Vec::new();
    for source in sources {
        let PatchSource::Remote(ref remote) = *source else {
            continue;
        };
        let written = source.to_string();
        let head = tree
            .patches
            .iter()
            .find(|digest| digest.source == written)
            .and_then(|digest| digest.head.as_deref());
        match remote.pull_status(head, node, upstream) {
            Ok(found) => pulls.extend(found),
            Err(err) => unchecked.push(format!("{written}: {err:#}")),
        }
    }
    (pulls, unchecked)
}

/// whether the patches pins.toml lists, or the files they read, no longer
/// match the ones the locked tree was built from, which eval refuses
pub fn drifted(project: &Project, sources: &[PatchSource], tree: &PatchedTree) -> bool {
    let listed = sources.iter().map(ToString::to_string);
    !listed.eq(tree.patches.iter().map(|digest| digest.source.clone()))
        || tree.patches.iter().any(|digest| {
            fs::read(project.dir().join(&digest.file))
                .is_ok_and(|bytes| HEXLOWER.encode(&Sha256::hash(&bytes)) != digest.sha256)
        })
}

/// what nix reports as a tarball's lastModified, since channel tarballs carry
/// no date and fetchTree would otherwise say 1970
fn newest_mtime(dir: &Path) -> Option<u64> {
    let mut newest = None;
    let mut pending = vec![dir.to_owned()];
    while let Some(next) = pending.pop() {
        let Ok(read) = fs::read_dir(&next) else {
            continue;
        };
        for entry in read.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            // unpacking a file into a directory stamps the directory with now
            if meta.is_dir() {
                pending.push(entry.path());
                continue;
            }
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_secs());
            newest = newest.max(modified);
        }
    }
    newest.filter(|&secs| secs > 0)
}

fn gcroot(project: &Project, name: &str) -> PathBuf {
    HistoryStore::for_project(project)
        .state_dir()
        .join("gcroots")
        .join(name)
}

/// points every gcroot at the tree the lock holds now, since undo and redo swap
/// the lock without touching the roots
pub fn reroot(project: &Project) -> Result<()> {
    let lock = project.load_lock()?;
    for (name, _) in lock.iter() {
        match lock.patched(name) {
            Some(tree) if tree.path.exists() => tree.path.root_at(&gcroot(project, name))?,
            Some(_) => {},
            None => unroot(project, name)?,
        }
    }
    Ok(())
}

/// drops the gcroot of a pin that no longer has a patched tree
pub fn unroot(project: &Project, name: &str) -> Result<()> {
    remove_stale(&gcroot(project, name))
}

/// drops the gcroots of pins that left pins.toml without `tack rm`
pub fn prune_roots(project: &Project, names: &[&str]) -> Result<()> {
    let dir = gcroot(project, "");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("read {}", dir.display())),
    };
    for listed in entries {
        let entry = listed?;
        if !names.iter().any(|name| entry.file_name() == **name) {
            remove_stale(&entry.path())?;
        }
    }
    Ok(())
}

fn remove_stale(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != ErrorKind::NotFound => {
            Err(err).with_context(|| format!("remove {}", path.display()))
        },
        _ => Ok(()),
    }
}
