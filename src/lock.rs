// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::BTreeMap,
    path::Path,
};

use misstep::Result;
use serde::{
    Deserialize,
    Deserializer,
    Serialize,
};
use serde_json::Value;

use crate::{
    patched::store::StorePath,
    project::write_atomic,
    source::{
        gitlab,
        normalize_host,
    },
};

pub const DECLARED_KEY: &str = "$declared";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LockFile {
    entries:     BTreeMap<String, Entry>,
    declared:    BTreeMap<String, String>,
    /// unknown nodes survive saves
    passthrough: BTreeMap<String, Value>,
}

/// tack's own fields sit beside the node, and the resolver strips them before
/// fetchTree sees it
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct Entry {
    #[serde(flatten)]
    node:      LockedNode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    patched:   Option<PatchedTree>,
    /// who signed the locked rev, which makes it the anchor later updates
    /// verify from
    #[serde(
        rename = "signedBy",
        default,
        deserialize_with = "lenient_signed_by",
        skip_serializing_if = "Option::is_none"
    )]
    signed_by: Option<SignedBy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tag:       Option<String>,
}

/// a record that doesn't parse reads as unsigned, so the node stays typed and
/// the next update verifies from scratch
fn lenient_signed_by<'de, D>(deserializer: D) -> Result<Option<SignedBy>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Value>::deserialize(deserializer)?
        .and_then(|value| SignedBy::deserialize(value).ok()))
}

/// the signer behind a pin's anchor, and what to check before trusting it again
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SignedBy {
    pub signer: String,
    /// digest of the signer's keys when the anchor was verified
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys:   Option<String>,
    /// the first rev verified under this anchor chain, so rollbacks to anything
    /// after it are known to be checked
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since:  Option<String>,
}

impl From<LockedNode> for Entry {
    fn from(node: LockedNode) -> Self {
        Self {
            node,
            patched: None,
            signed_by: None,
            tag: None,
        }
    }
}

impl LockFile {
    pub const fn new() -> Self {
        Self {
            entries:     BTreeMap::new(),
            declared:    BTreeMap::new(),
            passthrough: BTreeMap::new(),
        }
    }

    pub fn parse(raw: &str) -> Result<Self, serde_json::Error> {
        let values = serde_json::from_str::<BTreeMap<String, Value>>(raw)?;
        let mut lock = Self::new();
        for (name, value) in values {
            if name == DECLARED_KEY
                && let Ok(declared) = serde_json::from_value(value.clone())
            {
                lock.declared = declared;
            } else if let Ok(entry) = Entry::deserialize(&value) {
                lock.entries.insert(name, entry);
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
            .entries
            .iter()
            .map(|(name, entry)| (name.as_str(), NodeRepr::Typed(entry)));
        let kept = self
            .passthrough
            .iter()
            .map(|(name, value)| (name.as_str(), NodeRepr::Kept(value)));
        let declared = (!self.declared.is_empty())
            .then_some((DECLARED_KEY, NodeRepr::Declared(&self.declared)));
        typed.chain(kept).chain(declared).collect()
    }

    pub fn declared(&self, name: &str) -> Option<&str> {
        self.declared.get(name).map(String::as_str)
    }

    pub fn set_declared(&mut self, name: &str, url: &str) -> bool {
        if self.declared.get(name).is_some_and(|prev| prev == url) {
            return false;
        }
        self.declared.insert(name.to_owned(), url.to_owned());
        true
    }

    pub fn retain_declared<Keep>(&mut self, mut keep: Keep) -> bool
    where
        Keep: FnMut(&str) -> bool,
    {
        let before = self.declared.len();
        self.declared.retain(|name, _| keep(name));
        self.declared.len() != before
    }

    pub fn get(&self, name: &str) -> Option<&LockedNode> {
        self.entries.get(name).map(|entry| &entry.node)
    }

    /// a new node drops the signer, patched tree and tag, since none of them
    /// cover it
    pub fn insert(&mut self, name: String, node: LockedNode) -> Option<LockedNode> {
        self.passthrough.remove(&name);
        self.entries
            .insert(name, Entry::from(node))
            .map(|entry| entry.node)
    }

    pub fn signed_by(&self, name: &str) -> Option<&SignedBy> {
        self.entries.get(name)?.signed_by.as_ref()
    }

    pub fn set_signed_by(&mut self, name: &str, signer: Option<SignedBy>) -> bool {
        let Some(entry) = self.entries.get_mut(name) else {
            return false;
        };
        let changed = entry.signed_by != signer;
        entry.signed_by = signer;
        changed
    }

    pub fn patched(&self, name: &str) -> Option<&PatchedTree> {
        self.entries.get(name)?.patched.as_ref()
    }

    pub fn set_patched(&mut self, name: &str, tree: Option<PatchedTree>) -> bool {
        let Some(entry) = self.entries.get_mut(name) else {
            return false;
        };
        let changed = entry.patched != tree;
        entry.patched = tree;
        changed
    }

    pub fn tag(&self, name: &str) -> Option<&str> {
        self.entries.get(name)?.tag.as_deref()
    }

    pub fn set_tag(&mut self, name: &str, tag: Option<String>) -> bool {
        let Some(entry) = self.entries.get_mut(name) else {
            return false;
        };
        let changed = entry.tag != tag;
        entry.tag = tag;
        changed
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.declared.remove(name);
        let typed = self.entries.remove(name).is_some();
        let kept = self.passthrough.remove(name).is_some();
        typed || kept
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &LockedNode)> {
        self.entries.iter().map(|(name, entry)| (name, &entry.node))
    }

    pub fn unknown_nodes(&self) -> impl Iterator<Item = &str> {
        self.passthrough.keys().map(String::as_str)
    }

    pub fn unknown_type(&self, name: &str) -> Option<&str> {
        self.passthrough.get(name).map(lock_type)
    }

    pub fn unknown_nodes_with_values(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.passthrough
            .iter()
            .map(|(name, value)| (name.as_str(), value))
    }
}

enum NodeRepr<'a> {
    Typed(&'a Entry),
    Kept(&'a Value),
    Declared(&'a BTreeMap<String, String>),
}

impl Serialize for NodeRepr<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match *self {
            Self::Typed(entry) => entry.serialize(serializer),
            Self::Kept(value) => value.serialize(serializer),
            Self::Declared(declared) => declared.serialize(serializer),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PatchedTree {
    pub path:          StorePath,
    #[serde(rename = "narHash")]
    pub nar_hash:      String,
    #[serde(rename = "lastModified", skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<u64>,
    pub patches:       Vec<PatchDigest>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PatchDigest {
    pub source: String,
    /// where a remote patch was downloaded from, so repointing a shorturl
    /// alias downloads it again
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url:    Option<String>,
    pub file:   String,
    pub sha256: String,
    /// the pull request's head commit when it was vendored
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head:   Option<String>,
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

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn locked(&self, node: &str) -> Option<&LockedNode> {
        self.nodes.get(node)?.locked.known()
    }

    pub fn unknown_type(&self, node: &str) -> Option<&str> {
        match self.nodes.get(node)?.locked {
            LockedEntry::Unknown(ref kind) => Some(kind),
            LockedEntry::Absent | LockedEntry::Known(_) => None,
        }
    }

    pub fn inputs(&self, node: &str) -> impl Iterator<Item = (&str, &FlakeInputRef)> {
        self.nodes
            .get(node)
            .into_iter()
            .flat_map(|flake_node| &flake_node.inputs)
            .map(|(name, input)| (name.as_str(), input))
    }

    pub fn root_node(&self) -> Option<&FlakeNode> {
        self.nodes.get(&self.root)
    }

    pub fn node(&self, name: &str) -> Option<(&str, &FlakeNode)> {
        self.nodes
            .get_key_value(name)
            .map(|(stored, node)| (stored.as_str(), node))
    }
}

#[derive(Debug, Deserialize)]
pub struct FlakeNode {
    #[serde(default, deserialize_with = "deserialize_locked_node")]
    locked: LockedEntry,
    #[serde(default)]
    inputs: BTreeMap<String, FlakeInputRef>,
}

impl FlakeNode {
    pub const fn locked(&self) -> Option<&LockedNode> {
        self.locked.known()
    }

    pub fn inputs(&self) -> impl Iterator<Item = (&str, &FlakeInputRef)> {
        self.inputs
            .iter()
            .map(|(name, input_ref)| (name.as_str(), input_ref))
    }
}

/// a lock node's input names either another node or, as a list, the input
/// path it follows from the root
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum FlakeInputRef {
    Node(String),
    Follows(Vec<String>),
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

#[derive(Debug, Default)]
enum LockedEntry {
    #[default]
    Absent,
    Known(LockedNode),
    /// keeps the type, so a reader can name what it skipped
    Unknown(String),
}

impl LockedEntry {
    const fn known(&self) -> Option<&LockedNode> {
        match *self {
            Self::Known(ref node) => Some(node),
            Self::Absent | Self::Unknown(_) => None,
        }
    }
}

fn lock_type(value: &Value) -> &str {
    value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

fn deserialize_locked_node<'de, D>(deserializer: D) -> Result<LockedEntry, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(LockedEntry::Absent);
    };
    let kind = lock_type(&value).to_owned();
    Ok(LockedNode::from_value(value).map_or(LockedEntry::Unknown(kind), LockedEntry::Known))
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
