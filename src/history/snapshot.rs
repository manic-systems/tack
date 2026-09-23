// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::{
        BTreeMap,
        HashSet,
        hash_map::DefaultHasher,
    },
    fs,
    hash::{
        Hash as _,
        Hasher as _,
    },
    path::Path,
    result::Result as StdResult,
};

use data_encoding::{
    BASE64,
    HEXLOWER,
};
use hmac_sha256::Hash as Sha256;
use misstep::Result;
use serde::{
    Deserialize,
    Deserializer,
    Serialize,
    Serializer,
};

use super::Entry;
use crate::project::{
    self,
    Project,
};

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    toml:     Option<String>,
    lock:     Option<String>,
    resolver: Option<String>,
    /// the vendored patch and signer key files, which the lock and pins.toml
    /// point at
    files:    Option<String>,
}

impl Snapshot {
    pub fn capture(project: &Project) -> Self {
        Self {
            toml:     fs::read_to_string(project.pins_path()).ok(),
            lock:     fs::read_to_string(project.lock_path()).ok(),
            resolver: fs::read_to_string(project.resolver_path()).ok(),
            files:    capture_files(project.dir()),
        }
    }

    pub(super) fn into_entry(self, label: String, ts: u64) -> Entry {
        Entry {
            label,
            ts,
            toml: self.toml,
            lock: self.lock,
            resolver: self.resolver,
            files: self.files,
        }
    }

    pub(super) fn matches_entry(&self, entry: &Entry) -> bool {
        self.toml == entry.toml
            && self.lock == entry.lock
            && self.resolver == entry.resolver
            && self.files == entry.files
    }
}

/// the `.tack` subdirectories whose files undo restores with the state files
const VENDORED: [&str; 2] = ["patches", "keys"];

/// every vendored file keyed by its path under `.tack`, as one text blob so
/// undo stores it like the other state files
fn capture_files(tack: &Path) -> Option<String> {
    let files = vendored_files(tack);
    if files.is_empty() {
        return None;
    }
    let encoded = files
        .into_iter()
        .map(|(path, bytes)| (path, BASE64.encode(&bytes)))
        .collect::<BTreeMap<_, _>>();
    serde_json::to_string(&encoded).ok()
}

fn vendored_files(tack: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for dir in VENDORED {
        collect_files(tack, &tack.join(dir), &mut files);
    }
    files
}

fn collect_files(root: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_files(root, &path, files);
        } else if !kind.is_symlink()
            && let (Ok(relative), Ok(bytes)) = (path.strip_prefix(root), fs::read(&path))
        {
            files.insert(relative.to_string_lossy().into_owned(), bytes);
        }
    }
}

/// puts the vendored files back the way [`capture_files`] saw them
pub(super) fn restore_files(tack: &Path, blob: Option<&str>) -> Result<()> {
    let wanted = blob
        .map(serde_json::from_str::<BTreeMap<String, String>>)
        .transpose()?
        .unwrap_or_default();
    let present = vendored_files(tack);
    for path in present.keys().filter(|path| !wanted.contains_key(*path)) {
        fs::remove_file(tack.join(path))?;
    }
    for (path, encoded) in &wanted {
        let bytes = BASE64.decode(encoded.as_bytes())?;
        if present.get(path) == Some(&bytes) {
            continue;
        }
        let target = tack.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        project::write_atomic(&target, &bytes)?;
    }
    for dir in VENDORED {
        prune_empty(&tack.join(dir));
        let _ = fs::remove_dir(tack.join(dir));
    }
    Ok(())
}

fn prune_empty(dir: &Path) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            prune_empty(&entry.path());
            let _ = fs::remove_dir(entry.path());
        }
    }
}

#[derive(Deserialize, Serialize)]
pub(super) struct StoredHistory {
    #[serde(default)]
    cursor:  usize,
    #[serde(default)]
    entries: Vec<StoredEntry>,
}

impl StoredHistory {
    pub(super) const fn new(cursor: usize, entries: Vec<StoredEntry>) -> Self {
        Self { cursor, entries }
    }

    pub(super) fn into_parts(self) -> (usize, Vec<StoredEntry>) {
        (self.cursor, self.entries)
    }
}

#[derive(Deserialize, Serialize)]
pub(super) struct StoredEntry {
    #[serde(default)]
    label:    String,
    #[serde(default)]
    ts:       u64,
    #[serde(default)]
    toml:     SnapshotRef,
    #[serde(default)]
    lock:     SnapshotRef,
    #[serde(default)]
    resolver: SnapshotRef,
    #[serde(default)]
    files:    SnapshotRef,
}

impl StoredEntry {
    pub(super) const fn new(
        label: String,
        ts: u64,
        toml: SnapshotRef,
        lock: SnapshotRef,
        resolver: SnapshotRef,
        files: SnapshotRef,
    ) -> Self {
        Self {
            label,
            ts,
            toml,
            lock,
            resolver,
            files,
        }
    }

    pub(super) fn resolve(self, snaps: &Path) -> Option<Entry> {
        Some(Entry {
            label:    self.label,
            ts:       self.ts,
            toml:     self.toml.resolve(snaps)?.into_option(),
            lock:     self.lock.resolve(snaps)?.into_option(),
            resolver: self.resolver.resolve(snaps)?.into_option(),
            files:    self.files.resolve(snaps)?.into_option(),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum SnapshotRef {
    #[default]
    Missing,
    Present(String),
}

impl SnapshotRef {
    fn resolve(&self, snaps: &Path) -> Option<SnapshotBytes> {
        match *self {
            Self::Missing => Some(SnapshotBytes::Missing),
            Self::Present(ref name) => {
                fs::read_to_string(snaps.join(name))
                    .ok()
                    .map(SnapshotBytes::Present)
            },
        }
    }
}

enum SnapshotBytes {
    Missing,
    Present(String),
}

impl SnapshotBytes {
    fn into_option(self) -> Option<String> {
        match self {
            Self::Missing => None,
            Self::Present(content) => Some(content),
        }
    }
}

impl Serialize for SnapshotRef {
    fn serialize<S>(&self, serializer: S) -> StdResult<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *self {
            Self::Missing => serializer.serialize_none(),
            Self::Present(ref name) => serializer.serialize_some(name),
        }
    }
}

impl<'de> Deserialize<'de> for SnapshotRef {
    fn deserialize<D>(deserializer: D) -> StdResult<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Option::<String>::deserialize(deserializer)?.map_or(Self::Missing, Self::Present))
    }
}

/// stable across runs unlike the legacy hash
pub(super) fn content_key(text: &str) -> String {
    HEXLOWER.encode(&Sha256::hash(text.as_bytes()))
}

pub(super) fn legacy_content_key(text: &str) -> String {
    let mut hi = DefaultHasher::new();
    text.hash(&mut hi);
    let mut lo = DefaultHasher::new();
    0x9E37_79B9_7F4A_7C15_u64.hash(&mut lo);
    text.hash(&mut lo);
    format!("{:016x}{:016x}", hi.finish(), lo.finish())
}

pub(super) fn persist(
    snaps: &Path,
    content: Option<&str>,
    referenced: &mut HashSet<String>,
) -> Result<SnapshotRef> {
    let Some(text) = content else {
        return Ok(SnapshotRef::Missing);
    };
    let name = content_key(text);
    let path = snaps.join(&name);
    if !path.exists() {
        project::write_atomic(&path, text)?;
    }
    referenced.insert(name.clone());
    Ok(SnapshotRef::Present(name))
}

pub(super) fn sweep(snaps: &Path, referenced: &HashSet<String>) {
    let Ok(read) = fs::read_dir(snaps) else {
        return;
    };
    for entry in read.flatten() {
        let keep = entry
            .file_name()
            .to_str()
            .is_some_and(|name| referenced.contains(name));
        if !keep {
            let _ = fs::remove_file(entry.path());
        }
    }
}
