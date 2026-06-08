// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
};

use eyre::Result;
use serde::{
    Deserialize,
    Deserializer,
    Serialize,
};
use serde_json::Value;

use crate::source::{
    gitlab,
    normalize_host,
};

/// on-disk lock file keyed by input name
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LockFile {
    nodes:       BTreeMap<String, LockedNode>,
    /// unknown or missing-type nodes kept verbatim so save round-trips them
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
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, path)?;
        Ok(())
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
}

/// typed nodes serialize in field order, passthrough nodes verbatim
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

/// flake.lock exposed as locked nodes tack can compare
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
        path:  String,
        #[serde(flatten)]
        extra: ExtraFields,
    },
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
            nar_hash:      Some(nar_hash.into()),
            last_modified: Some(last_modified),
            extra:         BTreeMap::new(),
        }
    }

    pub fn new_path<P>(path: P) -> Self
    where
        P: Into<String>,
    {
        Self::Path {
            path:  path.into(),
            extra: BTreeMap::new(),
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

    pub fn rev(&self) -> Option<&str> {
        match self {
            Self::Tarball { url, .. } => Some(url),
            Self::Fixed { sha256, .. } => sha256.as_deref(),
            Self::Github { rev, .. } | Self::Gitlab { rev, .. } | Self::Git { rev, .. } => {
                rev.as_deref()
            },
            Self::Indirect { .. } | Self::Path { .. } => None,
        }
    }

    pub fn full_rev(&self) -> Option<&str> {
        match self {
            Self::Github { rev, .. } | Self::Gitlab { rev, .. } | Self::Git { rev, .. } => {
                rev.as_deref()
            },
            Self::Tarball { url, .. } => Some(url),
            Self::Fixed { url, sha256, .. } => url.as_deref().or(sha256.as_deref()),
            Self::Indirect { .. } | Self::Path { .. } => None,
        }
    }

    pub fn hash(&self) -> Option<&str> {
        match self {
            Self::Fixed { sha256, .. } => sha256.as_deref(),
            Self::Github { nar_hash, .. }
            | Self::Gitlab { nar_hash, .. }
            | Self::Git { nar_hash, .. }
            | Self::Tarball { nar_hash, .. } => nar_hash.as_deref(),
            Self::Indirect { .. } | Self::Path { .. } => None,
        }
    }

    pub fn last_modified(&self) -> Option<u64> {
        let value = match self {
            Self::Github { last_modified, .. }
            | Self::Gitlab { last_modified, .. }
            | Self::Git { last_modified, .. }
            | Self::Tarball { last_modified, .. } => *last_modified,
            Self::Fixed { .. } | Self::Indirect { .. } | Self::Path { .. } => None,
        }?;
        u64::try_from(value).ok()
    }
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

pub fn parse(raw: &str) -> Result<LockFile, serde_json::Error> {
    LockFile::parse(raw)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::{
        Value,
        json,
    };

    use super::{
        FlakeLock,
        LockFile,
        LockedNode,
    };

    fn node(value: Value) -> LockedNode {
        LockedNode::from_value(value).unwrap()
    }

    #[test]
    fn parse_skips_unknown_and_missing_type() {
        let raw = r#"{
            "good": {"type": "github", "owner": "o", "repo": "r", "rev": "abc"},
            "future": {"type": "mercurial", "url": "https://x"},
            "typeless": {"url": "https://y"}
        }"#;
        let lock = LockFile::parse(raw).unwrap();

        assert_eq!(lock.iter().count(), 1);
        assert!(lock.get("good").is_some());
        assert!(lock.get("future").is_none());

        let mut skipped = lock.unknown_nodes().collect::<Vec<_>>();
        skipped.sort_unstable();
        assert_eq!(skipped, vec!["future", "typeless"]);
    }

    #[test]
    fn save_round_trips_unknown_nodes() {
        let raw = r#"{
            "future": {"custom": true, "type": "mercurial"},
            "good": {"type": "github", "owner": "o", "repo": "r", "rev": "abc"}
        }"#;
        let lock = LockFile::parse(raw).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pins.lock.json");
        lock.save(&path).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        let back = serde_json::from_str::<Value>(&written).unwrap();
        assert_eq!(
            back.pointer("/future/type").and_then(Value::as_str),
            Some("mercurial")
        );
        assert_eq!(
            back.pointer("/future/custom").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            back.pointer("/good/owner").and_then(Value::as_str),
            Some("o")
        );

        let reparsed = LockFile::parse(&written).unwrap();
        assert_eq!(reparsed.unknown_nodes().collect::<Vec<_>>(), vec!["future"]);
        assert!(reparsed.get("good").is_some());
    }

    #[test]
    fn typed_only_save_is_byte_identical_to_legacy() {
        let raw = r#"{"a":{"type":"indirect","id":"nixpkgs"},"b":{"type":"github","owner":"o","repo":"r","rev":"abc"}}"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pins.lock.json");
        LockFile::parse(raw).unwrap().save(&path).unwrap();
        let written = fs::read_to_string(&path).unwrap();

        let mut legacy_nodes = super::BTreeMap::new();
        legacy_nodes.insert(
            "a".to_owned(),
            node(json!({"type": "indirect", "id": "nixpkgs"})),
        );
        legacy_nodes.insert(
            "b".to_owned(),
            node(json!({"type": "github", "owner": "o", "repo": "r", "rev": "abc"})),
        );
        let mut legacy = serde_json::to_string_pretty(&legacy_nodes).unwrap();
        legacy.push('\n');

        assert_eq!(written, legacy);
    }

    #[test]
    fn remove_drops_unknown_node_and_insert_supersedes_it() {
        let raw = r#"{"x": {"type": "mercurial", "url": "https://x"}}"#;
        let mut lock = LockFile::parse(raw).unwrap();
        assert_eq!(lock.unknown_nodes().count(), 1);

        lock.insert(
            "x".to_owned(),
            node(json!({"type": "github", "owner": "o", "repo": "r"})),
        );
        assert_eq!(lock.unknown_nodes().count(), 0);
        assert!(lock.get("x").is_some());

        let mut kept = LockFile::parse(raw).unwrap();
        assert!(kept.remove("x"));
        assert!(!kept.remove("x"));
        assert_eq!(kept.unknown_nodes().count(), 0);
    }

    #[test]
    fn rev_uses_type_specific_identity_key() {
        assert_eq!(
            node(json!({"type": "github", "rev": "abc", "owner": "o", "repo": "r"})).rev(),
            Some("abc")
        );
        assert_eq!(
            node(json!({"type": "tarball", "url": "https://x/y"})).rev(),
            Some("https://x/y")
        );
        assert_eq!(
            node(json!({"type": "fixed", "url": "https://x", "sha256": "sha256-z"})).rev(),
            Some("sha256-z")
        );
    }

    #[test]
    fn full_rev_prefers_url_for_fixed_nodes() {
        let fixed = node(json!({"type": "fixed", "url": "https://x", "sha256": "sha256-z"}));
        assert_eq!(fixed.full_rev(), Some("https://x"));
        assert_eq!(fixed.rev(), Some("sha256-z"));
    }

    #[test]
    fn hash_uses_sha256_for_fixed_else_nar_hash() {
        assert_eq!(
            node(json!({"type": "fixed", "sha256": "sha256-z"})).hash(),
            Some("sha256-z")
        );
        assert_eq!(
            node(json!({"type": "github", "owner": "o", "repo": "r", "narHash": "sha256-n"}))
                .hash(),
            Some("sha256-n")
        );
    }

    #[test]
    fn last_modified_reads_positive_epoch() {
        assert_eq!(
            node(json!({"type": "github", "owner": "o", "repo": "r", "lastModified": 1700_i64}))
                .last_modified(),
            Some(1700)
        );
        assert_eq!(
            node(json!({"type": "github", "owner": "o", "repo": "r"})).last_modified(),
            None
        );
    }

    #[test]
    fn typed_nodes_cover_flake_lock_node_shapes() {
        let gitlab = node(json!({"type": "gitlab", "owner": "o", "repo": "r"}));
        assert!(matches!(
            gitlab,
            LockedNode::Gitlab { ref host, .. } if host == "gitlab.com"
        ));
        assert!(matches!(
            node(json!({"type": "indirect", "id": "nixpkgs"})),
            LockedNode::Indirect { ref id, .. } if id == "nixpkgs"
        ));
        assert!(matches!(
            node(json!({"type": "path", "path": "/p"})),
            LockedNode::Path { ref path, .. } if path == "/p"
        ));
    }

    #[test]
    fn gitlab_host_is_canonicalized_at_lock_boundary() {
        let parsed = node(json!({
            "type": "gitlab",
            "host": "GITLAB.COM:443",
            "owner": "o",
            "repo": "r"
        }));
        assert!(matches!(
            parsed,
            LockedNode::Gitlab { ref host, .. } if host == "gitlab.com"
        ));

        let default_host = serde_json::to_value(LockedNode::new_gitlab(
            "GitLab.Com:443",
            "o",
            "r",
            "rev",
            "sha256-n",
            10,
        ))
        .unwrap();
        assert!(default_host.get("host").is_none());

        let self_hosted = serde_json::to_value(LockedNode::new_gitlab(
            "GitLab.Example.Com:8443",
            "o",
            "r",
            "rev",
            "sha256-n",
            10,
        ))
        .unwrap();
        assert_eq!(
            self_hosted.get("host").and_then(Value::as_str),
            Some("gitlab.example.com:8443")
        );
    }

    #[test]
    fn flake_lock_locked_nodes_skip_root_and_unknown_locked_nodes() {
        let raw = r#"{
            "root": "root",
            "nodes": {
                "root": {},
                "empty": {},
                "future": {"locked": {"type": "future", "x": 1}},
                "nixpkgs": {
                    "locked": {
                        "type": "github",
                        "owner": "NixOS",
                        "repo": "nixpkgs",
                        "rev": "abc"
                    }
                }
            }
        }"#;

        let doc = FlakeLock::parse(raw).unwrap();
        let nodes = doc
            .locked_nodes()
            .map(|(name, node)| {
                let repo = match *node {
                    LockedNode::Github { ref repo, .. } => Some(repo.to_owned()),
                    LockedNode::Gitlab { .. }
                    | LockedNode::Git { .. }
                    | LockedNode::Tarball { .. }
                    | LockedNode::Fixed { .. }
                    | LockedNode::Indirect { .. }
                    | LockedNode::Path { .. } => None,
                };
                (name.to_owned(), repo)
            })
            .collect::<Vec<_>>();

        assert_eq!(nodes, vec![(
            "nixpkgs".to_owned(),
            Some("nixpkgs".to_owned())
        )]);
    }

    #[test]
    fn roundtrip_preserves_extra_lock_fields() {
        // github node with a field tack does not model
        let raw = r#"{"type":"github","owner":"o","repo":"r","ref":"nixos-unstable","rev":"abc","narHash":"sha256-z","lastModified":1700,"revCount":42}"#;
        let n = LockedNode::from_value(serde_json::from_str(raw).unwrap()).unwrap();
        let back = serde_json::to_string(&n).unwrap();
        println!("IN : {raw}");
        println!("OUT: {back}");
        let back_json = serde_json::from_str::<Value>(&back).unwrap();
        assert_eq!(back_json.get("ref"), Some(&json!("nixos-unstable")));
        assert_eq!(back_json.get("revCount"), Some(&json!(42_i64)));
    }

    #[test]
    fn typed_git_node_omits_false_submodules() {
        let node = serde_json::to_value(LockedNode::new_git(
            "https://x",
            "refs/heads/main",
            "rev",
            "sha256-n",
            10,
            false,
        ))
        .unwrap();

        assert_eq!(node.get("type").and_then(Value::as_str), Some("git"));
        assert!(node.get("submodules").is_none());
    }

    #[test]
    fn typed_git_node_keeps_true_submodules() {
        let node = serde_json::to_value(LockedNode::new_git(
            "https://x",
            "refs/heads/main",
            "rev",
            "sha256-n",
            10,
            true,
        ))
        .unwrap();

        assert_eq!(node.get("submodules").and_then(Value::as_bool), Some(true));
    }
}
