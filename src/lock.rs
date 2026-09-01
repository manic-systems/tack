// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::BTreeMap,
    path::Path,
};

use eyre::Result;
use serde::{
    Deserialize,
    Deserializer,
    Serialize,
};
use serde_json::Value;

use crate::{
    project::write_atomic,
    source::{
        gitlab,
        normalize_host,
    },
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LockFile {
    nodes:       BTreeMap<String, LockedNode>,
    /// unknown nodes survive saves
    passthrough: BTreeMap<String, Value>,
}

impl LockFile {
    pub const fn new() -> Self {
        Self {
            nodes:       BTreeMap::new(),
            passthrough: BTreeMap::new(),
        }
    }

    pub fn parse(raw: &str) -> Result<Self, serde_json::Error> {
        let entries = serde_json::from_str::<BTreeMap<String, Value>>(raw)?;
        let mut lock = Self::new();
        for (name, value) in entries {
            if let Ok(node) = LockedNode::from_value(value.clone()) {
                lock.nodes.insert(name, node);
            } else {
                lock.passthrough.insert(name, value);
            }
        }
        Ok(lock)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut json = serde_json::to_string_pretty(&self.merged())?;
        json.push('\n');
        write_atomic(path, &json)
    }

    fn merged(&self) -> BTreeMap<&str, NodeRepr<'_>> {
        let typed = self
            .nodes
            .iter()
            .map(|(name, node)| (name.as_str(), NodeRepr::Typed(node)));
        let kept = self
            .passthrough
            .iter()
            .map(|(name, value)| (name.as_str(), NodeRepr::Kept(value)));
        typed.chain(kept).collect()
    }

    pub fn get(&self, name: &str) -> Option<&LockedNode> {
        self.nodes.get(name)
    }

    pub fn insert(&mut self, name: String, node: LockedNode) -> Option<LockedNode> {
        self.passthrough.remove(&name);
        self.nodes.insert(name, node)
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let typed = self.nodes.remove(name).is_some();
        let kept = self.passthrough.remove(name).is_some();
        typed || kept
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.nodes.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &LockedNode)> {
        self.nodes.iter()
    }

    pub fn unknown_nodes(&self) -> impl Iterator<Item = &str> {
        self.passthrough.keys().map(String::as_str)
    }

    pub fn unknown_nodes_with_values(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.passthrough
            .iter()
            .map(|(name, value)| (name.as_str(), value))
    }
}

enum NodeRepr<'a> {
    Typed(&'a LockedNode),
    Kept(&'a Value),
}

impl Serialize for NodeRepr<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match *self {
            Self::Typed(node) => node.serialize(serializer),
            Self::Kept(value) => value.serialize(serializer),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct FlakeLock {
    #[serde(default = "default_root")]
    root:  String,
    #[serde(default)]
    nodes: BTreeMap<String, FlakeNode>,
}

impl FlakeLock {
    pub fn parse(raw: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(raw)
    }

    pub fn locked_nodes(&self) -> impl Iterator<Item = (&str, &LockedNode)> {
        let root = self.root.as_str();
        self.nodes.iter().filter_map(move |(name, node)| {
            if name == root {
                return None;
            }
            Some((name.as_str(), node.locked.as_ref()?))
        })
    }
}

#[derive(Debug, Deserialize)]
struct FlakeNode {
    #[serde(default, deserialize_with = "deserialize_locked_node")]
    locked: Option<LockedNode>,
}

type ExtraFields = BTreeMap<String, Value>;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum LockedNode {
    #[serde(rename = "github")]
    Github {
        owner:         String,
        repo:          String,
        #[serde(skip_serializing_if = "Option::is_none")]
        rev:           Option<String>,
        #[serde(rename = "narHash", skip_serializing_if = "Option::is_none")]
        nar_hash:      Option<String>,
        #[serde(rename = "lastModified", skip_serializing_if = "Option::is_none")]
        last_modified: Option<i64>,
        #[serde(flatten)]
        extra:         ExtraFields,
    },
    #[serde(rename = "gitlab")]
    Gitlab {
        owner:         String,
        repo:          String,
        #[serde(
            default = "default_gitlab_host",
            deserialize_with = "deserialize_host",
            skip_serializing_if = "is_default_gitlab_host"
        )]
        host:          String,
        #[serde(skip_serializing_if = "Option::is_none")]
        rev:           Option<String>,
        #[serde(rename = "narHash", skip_serializing_if = "Option::is_none")]
        nar_hash:      Option<String>,
        #[serde(rename = "lastModified", skip_serializing_if = "Option::is_none")]
        last_modified: Option<i64>,
        #[serde(flatten)]
        extra:         ExtraFields,
    },
    #[serde(rename = "git")]
    Git {
        url:           String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        reff:          Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        rev:           Option<String>,
        #[serde(rename = "narHash", skip_serializing_if = "Option::is_none")]
        nar_hash:      Option<String>,
        #[serde(rename = "lastModified", skip_serializing_if = "Option::is_none")]
        last_modified: Option<i64>,
        #[serde(default, skip_serializing_if = "is_false")]
        submodules:    bool,
        #[serde(flatten)]
        extra:         ExtraFields,
    },
    #[serde(rename = "tarball")]
    Tarball {
        url:           String,
        #[serde(skip_serializing_if = "Option::is_none")]
        rev:           Option<String>,
        #[serde(rename = "narHash", skip_serializing_if = "Option::is_none")]
        nar_hash:      Option<String>,
        #[serde(rename = "lastModified", skip_serializing_if = "Option::is_none")]
        last_modified: Option<i64>,
        #[serde(flatten)]
        extra:         ExtraFields,
    },
    #[serde(rename = "fixed")]
    Fixed {
        #[serde(skip_serializing_if = "Option::is_none")]
        url:    Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        unpack: Option<String>,
        #[serde(flatten)]
        extra:  ExtraFields,
    },
    #[serde(rename = "indirect")]
    Indirect {
        id:    String,
        #[serde(flatten)]
        extra: ExtraFields,
    },
    #[serde(rename = "path")]
    Path {
        path:          String,
        #[serde(rename = "narHash", skip_serializing_if = "Option::is_none")]
        nar_hash:      Option<String>,
        #[serde(rename = "lastModified", skip_serializing_if = "Option::is_none")]
        last_modified: Option<i64>,
        #[serde(rename = "mtimeNanos", skip_serializing_if = "Option::is_none")]
        mtime_nanos:   Option<i64>,
        #[serde(rename = "treeSize", skip_serializing_if = "Option::is_none")]
        tree_size:     Option<u64>,
        #[serde(rename = "treeEntries", skip_serializing_if = "Option::is_none")]
        tree_entries:  Option<u64>,
        #[serde(flatten)]
        extra:         ExtraFields,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockIdentity<'a> {
    Rev(&'a str),
    ContentHash(&'a str),
    ImmutableUrl(&'a str),
    SourceUrl(&'a str),
    PathFingerprint(String),
}

impl LockIdentity<'_> {
    pub const fn as_str(&self) -> &str {
        match *self {
            Self::Rev(value)
            | Self::ContentHash(value)
            | Self::ImmutableUrl(value)
            | Self::SourceUrl(value) => value,
            Self::PathFingerprint(ref value) => value.as_str(),
        }
    }

    pub fn into_string(self) -> String {
        match self {
            Self::Rev(value)
            | Self::ContentHash(value)
            | Self::ImmutableUrl(value)
            | Self::SourceUrl(value) => value.to_owned(),
            Self::PathFingerprint(value) => value,
        }
    }
}

impl AsRef<str> for LockIdentity<'_> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[expect(
    clippy::pattern_type_mismatch,
    reason = "these accessors borrow fields out of an enum behind &self"
)]
impl LockedNode {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }

    pub fn new_github<Owner, Repo, Rev, NarHash>(
        owner: Owner,
        repo: Repo,
        rev: Rev,
        nar_hash: NarHash,
        last_modified: i64,
    ) -> Self
    where
        Owner: Into<String>,
        Repo: Into<String>,
        Rev: Into<String>,
        NarHash: Into<String>,
    {
        Self::Github {
            owner:         owner.into(),
            repo:          repo.into(),
            rev:           Some(rev.into()),
            nar_hash:      Some(nar_hash.into()),
            last_modified: Some(last_modified),
            extra:         BTreeMap::new(),
        }
    }

    pub fn new_gitlab<Host, Owner, Repo, Rev, NarHash>(
        host: Host,
        owner: Owner,
        repo: Repo,
        rev: Rev,
        nar_hash: NarHash,
        last_modified: i64,
    ) -> Self
    where
        Host: Into<String>,
        Owner: Into<String>,
        Repo: Into<String>,
        Rev: Into<String>,
        NarHash: Into<String>,
    {
        let raw_host = host.into();
        let canonical_host = normalize_host(&raw_host);
        Self::Gitlab {
            host:          canonical_host,
            owner:         owner.into(),
            repo:          repo.into(),
            rev:           Some(rev.into()),
            nar_hash:      Some(nar_hash.into()),
            last_modified: Some(last_modified),
            extra:         BTreeMap::new(),
        }
    }

    pub fn new_git<Url, Ref, Rev, NarHash>(
        url: Url,
        reff: Ref,
        rev: Rev,
        nar_hash: NarHash,
        last_modified: i64,
        submodules: bool,
    ) -> Self
    where
        Url: Into<String>,
        Ref: Into<String>,
        Rev: Into<String>,
        NarHash: Into<String>,
    {
        Self::Git {
            url: url.into(),
            reff: Some(reff.into()),
            rev: Some(rev.into()),
            nar_hash: Some(nar_hash.into()),
            last_modified: Some(last_modified),
            submodules,
            extra: BTreeMap::new(),
        }
    }

    pub fn new_tarball<Url, NarHash>(url: Url, nar_hash: NarHash, last_modified: i64) -> Self
    where
        Url: Into<String>,
        NarHash: Into<String>,
    {
        Self::Tarball {
            url:           url.into(),
            rev:           None,
            nar_hash:      Some(nar_hash.into()),
            last_modified: Some(last_modified),
            extra:         BTreeMap::new(),
        }
    }

    // channel tarballs ship their rev, fetchTree derives lastModified itself
    pub fn new_tarball_with_rev<Url, Rev, NarHash>(url: Url, rev: Rev, nar_hash: NarHash) -> Self
    where
        Url: Into<String>,
        Rev: Into<String>,
        NarHash: Into<String>,
    {
        Self::Tarball {
            url:           url.into(),
            rev:           Some(rev.into()),
            nar_hash:      Some(nar_hash.into()),
            last_modified: None,
            extra:         BTreeMap::new(),
        }
    }

    pub fn new_path<P>(path: P, nar_hash: Option<String>) -> Self
    where
        P: Into<String>,
    {
        Self::Path {
            path: path.into(),
            nar_hash,
            last_modified: None,
            mtime_nanos: None,
            tree_size: None,
            tree_entries: None,
            extra: BTreeMap::new(),
        }
    }

    pub fn new_path_with_fingerprint<P>(path: P, fingerprint: PathFingerprint) -> Self
    where
        P: Into<String>,
    {
        Self::Path {
            path:          path.into(),
            nar_hash:      None,
            last_modified: Some(fingerprint.last_modified),
            mtime_nanos:   Some(fingerprint.mtime_nanos),
            tree_size:     Some(fingerprint.tree_size),
            tree_entries:  Some(fingerprint.tree_entries),
            extra:         BTreeMap::new(),
        }
    }

    pub fn new_fixed<Url, Sha256, Unpack>(url: Url, sha256: Sha256, unpack: Unpack) -> Self
    where
        Url: Into<String>,
        Sha256: Into<String>,
        Unpack: Into<String>,
    {
        Self::Fixed {
            url:    Some(url.into()),
            sha256: Some(sha256.into()),
            unpack: Some(unpack.into()),
            extra:  BTreeMap::new(),
        }
    }

    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Github { .. } => "github",
            Self::Gitlab { .. } => "gitlab",
            Self::Git { .. } => "git",
            Self::Tarball { .. } => "tarball",
            Self::Fixed { .. } => "fixed",
            Self::Indirect { .. } => "indirect",
            Self::Path { .. } => "path",
        }
    }

    pub fn resolved_identity(&self) -> Option<LockIdentity<'_>> {
        match self {
            Self::Tarball { url, .. } => Some(LockIdentity::ImmutableUrl(url)),
            Self::Fixed { sha256, .. } => sha256.as_deref().map(LockIdentity::ContentHash),
            Self::Github { rev, .. } | Self::Gitlab { rev, .. } | Self::Git { rev, .. } => {
                rev.as_deref().map(LockIdentity::Rev)
            },
            Self::Path {
                path,
                mtime_nanos: Some(mtime_nanos),
                tree_size: Some(tree_size),
                tree_entries: Some(tree_entries),
                ..
            } => {
                let fingerprint =
                    path_fingerprint_identity(path, *mtime_nanos, *tree_size, *tree_entries);
                Some(LockIdentity::PathFingerprint(fingerprint))
            },
            Self::Indirect { .. } | Self::Path { .. } => None,
        }
    }

    pub fn source_identity(&self) -> Option<LockIdentity<'_>> {
        match self {
            Self::Github { rev, .. } | Self::Gitlab { rev, .. } | Self::Git { rev, .. } => {
                rev.as_deref().map(LockIdentity::Rev)
            },
            Self::Tarball { url, rev, .. } => {
                rev.as_deref()
                    .map(LockIdentity::Rev)
                    .or(Some(LockIdentity::ImmutableUrl(url)))
            },
            Self::Fixed { url, sha256, .. } => {
                url.as_deref()
                    .map(LockIdentity::SourceUrl)
                    .or_else(|| sha256.as_deref().map(LockIdentity::ContentHash))
            },
            Self::Path {
                path,
                mtime_nanos: Some(mtime_nanos),
                tree_size: Some(tree_size),
                tree_entries: Some(tree_entries),
                ..
            } => {
                let fingerprint =
                    path_fingerprint_identity(path, *mtime_nanos, *tree_size, *tree_entries);
                Some(LockIdentity::PathFingerprint(fingerprint))
            },
            Self::Indirect { .. } | Self::Path { .. } => None,
        }
    }

    pub fn forge_rev(&self) -> Option<&str> {
        match self {
            Self::Github { rev, .. } | Self::Gitlab { rev, .. } | Self::Git { rev, .. } => {
                rev.as_deref()
            },
            Self::Tarball { .. }
            | Self::Fixed { .. }
            | Self::Indirect { .. }
            | Self::Path { .. } => None,
        }
    }

    pub fn hash(&self) -> Option<&str> {
        match self {
            Self::Fixed { sha256, .. } => sha256.as_deref(),
            Self::Github { nar_hash, .. }
            | Self::Gitlab { nar_hash, .. }
            | Self::Git { nar_hash, .. }
            | Self::Tarball { nar_hash, .. }
            | Self::Path { nar_hash, .. } => nar_hash.as_deref(),
            Self::Indirect { .. } => None,
        }
    }

    pub fn last_modified(&self) -> Option<u64> {
        let value = match self {
            Self::Github { last_modified, .. }
            | Self::Gitlab { last_modified, .. }
            | Self::Git { last_modified, .. }
            | Self::Tarball { last_modified, .. }
            | Self::Path { last_modified, .. } => *last_modified,
            Self::Fixed { .. } | Self::Indirect { .. } => None,
        }?;
        u64::try_from(value).ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathFingerprint {
    pub last_modified: i64,
    pub mtime_nanos:   i64,
    pub tree_size:     u64,
    pub tree_entries:  u64,
}

fn path_fingerprint_identity(
    path: &str,
    mtime_nanos: i64,
    tree_size: u64,
    tree_entries: u64,
) -> String {
    format!("path:{path}:{mtime_nanos}:{tree_size}:{tree_entries}")
}

fn default_gitlab_host() -> String {
    "gitlab.com".to_owned()
}

fn default_root() -> String {
    "root".to_owned()
}

fn deserialize_locked_node<'de, D>(deserializer: D) -> Result<Option<LockedNode>, D::Error>
where
    D: Deserializer<'de>,
{
    let locked = Option::<Value>::deserialize(deserializer)?;
    Ok(locked.and_then(|value| LockedNode::from_value(value).ok()))
}

fn deserialize_host<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(|host| normalize_host(&host))
}

fn is_default_gitlab_host(host: &str) -> bool {
    gitlab::is_default_host(host)
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires a borrowed field"
)]
const fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
#[path = "lock_tests.rs"]
mod tests;
