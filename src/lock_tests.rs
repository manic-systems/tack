// SPDX-License-Identifier: EUPL-1.2

use std::fs;

use serde_json::{
    Value,
    json,
};

use super::{
    LockFile,
    LockedNode,
};

#[test]
fn save_preserves_unknown_lock_nodes() {
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
        back.pointer("/future/custom").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        LockFile::parse(&written)
            .unwrap()
            .unknown_nodes()
            .collect::<Vec<_>>(),
        vec!["future"]
    );
}

#[test]
fn extra_lock_fields_survive_node_roundtrip() {
    let raw = r#"{"type":"github","owner":"o","repo":"r","ref":"nixos-unstable","rev":"abc","narHash":"sha256-z","lastModified":1700,"revCount":42}"#;
    let node = LockedNode::from_value(serde_json::from_str(raw).unwrap()).unwrap();
    let back = serde_json::to_value(&node).unwrap();

    assert_eq!(back.get("ref"), Some(&json!("nixos-unstable")));
    assert_eq!(back.get("revCount"), Some(&json!(42_i64)));
}

#[test]
fn public_is_written_only_when_set() {
    let mut node = LockedNode::new_github("o", "r", "abc", "sha256-z", 1700);
    assert_eq!(serde_json::to_value(&node).unwrap().get("public"), None);

    node.set_public();
    let back = LockedNode::from_value(serde_json::to_value(&node).unwrap()).unwrap();
    assert!(back.is_public());
}
