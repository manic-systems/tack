// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::{
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

use data_encoding::HEXLOWER;
use eyre::Result;
use hmac_sha256::Hash as Sha256;
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
}

impl Snapshot {
    pub fn capture(project: &Project) -> Self {
        Self {
            toml:     fs::read_to_string(project.pins_path()).ok(),
            lock:     fs::read_to_string(project.lock_path()).ok(),
            resolver: fs::read_to_string(project.resolver_path()).ok(),
        }
    }

    pub(super) fn into_entry(self, label: String, ts: u64) -> Entry {
        Entry {
            label,
            ts,
            toml: self.toml,
            lock: self.lock,
            resolver: self.resolver,
        }
    }

    pub(super) fn matches_entry(&self, entry: &Entry) -> bool {
        self.toml == entry.toml && self.lock == entry.lock && self.resolver == entry.resolver
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
}

impl StoredEntry {
    pub(super) const fn new(
        label: String,
        ts: u64,
        toml: SnapshotRef,
        lock: SnapshotRef,
        resolver: SnapshotRef,
    ) -> Self {
        Self {
            label,
            ts,
            toml,
            lock,
            resolver,
        }
    }

    pub(super) fn resolve(self, snaps: &Path) -> Option<Entry> {
        Some(Entry {
            label:    self.label,
            ts:       self.ts,
            toml:     self.toml.resolve(snaps)?.into_option(),
            lock:     self.lock.resolve(snaps)?.into_option(),
            resolver: self.resolver.resolve(snaps)?.into_option(),
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
