// SPDX-License-Identifier: EUPL-1.2

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// name -> locked node; btreemap keeps the file sorted
pub type Lock = BTreeMap<String, Value>;

pub fn load(path: &Path) -> Result<Lock> {
    if !path.exists() {
        return Ok(Lock::new());
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

pub fn save(path: &Path, lock: &Lock) -> Result<()> {
    let mut json = serde_json::to_string_pretty(lock)?;
    json.push('\n');
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn rev_of(node: &Value) -> Option<&str> {
    if node.get("type").and_then(Value::as_str) == Some("tarball") {
        node.get("url")?.as_str()
    } else {
        node.get("rev")?.as_str()
    }
}

pub fn hash_of(node: &Value) -> Option<&str> {
    node.get("narHash")?.as_str()
}
