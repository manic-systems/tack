// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
};

use eyre::Result;
use serde_json::Value;

/// name -> locked node; btreemap keeps the file sorted
pub type Lock = BTreeMap<String, Value>;

/// a typed, read-only view over a single lock entry. storage stays
/// `serde_json::Value` so the lock file's exact bytes are never touched. this
/// is only a funnel for the accessors that used to poke at string keys all over
/// the command layer and lock helpers.
#[derive(Clone, Copy)]
pub struct Node<'a>(&'a Value);

impl<'a> From<&'a Value> for Node<'a> {
    fn from(value: &'a Value) -> Self {
        Self(value)
    }
}

impl<'a> Node<'a> {
    fn str(self, key: &str) -> Option<&'a str> {
        self.0.get(key).and_then(Value::as_str)
    }

    /// the locked node's `type` discriminant, when present
    pub fn kind(self) -> Option<&'a str> {
        self.str("type")
    }

    /// the identity rev for lock bookkeeping: `url` for tarball, `sha256` for
    /// fixed, else `rev`
    pub fn rev(self) -> Option<&'a str> {
        match self.kind() {
            Some("tarball") => self.str("url"),
            Some("fixed") => self.str("sha256"),
            _ => self.str("rev"),
        }
    }

    /// content hash: `sha256` for fixed, else `narHash`
    pub fn hash(self) -> Option<&'a str> {
        match self.kind() {
            Some("fixed") => self.str("sha256"),
            _ => self.str("narHash"),
        }
    }

    /// the untruncated identity used for dedup display and grouping: first of
    /// `rev`, `url`, `sha256`. note this differs from [`Node::rev`] for fixed
    /// nodes (url here, sha256 there) because both behaviours are load-bearing.
    pub fn full_rev(self) -> Option<&'a str> {
        ["rev", "url", "sha256"]
            .into_iter()
            .find_map(|key| self.str(key))
    }

    pub fn last_modified(self) -> Option<u64> {
        self.0.get("lastModified").and_then(Value::as_u64)
    }
}

/// parse pins.lock.json from an in-memory string
pub fn parse(raw: &str) -> Result<Lock, serde_json::Error> {
    serde_json::from_str(raw)
}

pub fn save(path: &Path, lock: &Lock) -> Result<()> {
    let mut json = serde_json::to_string_pretty(lock)?;
    json.push('\n');
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Node;

    #[test]
    fn rev_uses_type_specific_identity_key() {
        assert_eq!(
            Node::from(&json!({"type": "github", "rev": "abc"})).rev(),
            Some("abc")
        );
        assert_eq!(
            Node::from(&json!({"type": "tarball", "url": "https://x/y"})).rev(),
            Some("https://x/y")
        );
        assert_eq!(
            Node::from(&json!({"type": "fixed", "url": "https://x", "sha256": "sha256-z"})).rev(),
            Some("sha256-z")
        );
    }

    #[test]
    fn full_rev_prefers_rev_then_url_then_sha256() {
        let fixed = json!({"type": "fixed", "url": "https://x", "sha256": "sha256-z"});
        assert_eq!(Node::from(&fixed).full_rev(), Some("https://x"));
        assert_eq!(Node::from(&fixed).rev(), Some("sha256-z"));
        assert_eq!(Node::from(&json!({"rev": "abc"})).full_rev(), Some("abc"));
    }

    #[test]
    fn hash_uses_sha256_for_fixed_else_nar_hash() {
        assert_eq!(
            Node::from(&json!({"type": "fixed", "sha256": "sha256-z"})).hash(),
            Some("sha256-z")
        );
        assert_eq!(
            Node::from(&json!({"type": "github", "narHash": "sha256-n"})).hash(),
            Some("sha256-n")
        );
    }

    #[test]
    fn last_modified_reads_u64() {
        assert_eq!(
            Node::from(&json!({"lastModified": 1700_u64})).last_modified(),
            Some(1700)
        );
        assert_eq!(Node::from(&json!({})).last_modified(), None);
    }
}
