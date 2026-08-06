// SPDX-License-Identifier: EUPL-1.2

use std::fs;

use serde_json::{
    Value,
    json,
};

use super::{
    FlakeInputRef,
    FlakeLock,
    FlakeLockError,
    LockFile,
    LockedNode,
};

fn node(value: Value) -> LockedNode {
    LockedNode::from_value(value).unwrap()
}

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
fn remove_and_insert_replace_unknown_nodes() {
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
    assert_eq!(kept.unknown_nodes().count(), 0);
}

#[test]
fn extra_lock_fields_survive_node_roundtrip() {
    let raw = r#"{"type":"github","owner":"o","repo":"r","ref":"nixos-unstable","rev":"abc","narHash":"sha256-z","lastModified":1700,"revCount":42}"#;
    let node = LockedNode::from_value(serde_json::from_str(raw).unwrap()).unwrap();
    let back = serde_json::to_value(&node).unwrap();

    assert_eq!(back.get("ref"), Some(&json!("nixos-unstable")));
    assert_eq!(back.get("revCount"), Some(&json!(42_i64)));
}

fn flake_lock(nodes: &Value) -> FlakeLock {
    FlakeLock::parse(
        &json!({
            "nodes": nodes,
            "root": "root",
            "version": 7,
        })
        .to_string(),
    )
    .unwrap()
}

#[test]
fn flake_lock_exposes_root_nodes_and_direct_inputs() {
    let lock = flake_lock(&json!({
        "dep": {"locked": {"type": "github", "owner": "o", "repo": "r"}},
        "root": {"inputs": {"dep": "dep"}},
    }));

    assert_eq!(lock.root_name(), "root");
    assert!(lock.root().is_some());
    assert!(lock.node("root").is_some());
    assert!(lock.node("dep").unwrap().locked().is_some());
    assert_eq!(lock.root().unwrap().inputs().collect::<Vec<_>>(), vec![(
        "dep",
        &FlakeInputRef::Node("dep".to_owned())
    )]);

    let (name, node) = lock
        .resolve_input_ref(lock.root().unwrap().input("dep").unwrap())
        .unwrap();
    assert_eq!(name, "dep");
    assert!(node.locked().is_some());
}

#[test]
fn flake_lock_resolves_follows_paths_from_root() {
    let lock = flake_lock(&json!({
        "dep": {"locked": {"type": "github", "owner": "o", "repo": "dep"}},
        "root": {"inputs": {"alias": ["dep"], "dep": "dep"}},
    }));

    let alias = lock.root().unwrap().input("alias").unwrap();
    assert_eq!(alias, &FlakeInputRef::Follows(vec!["dep".to_owned()]));
    assert_eq!(lock.resolve_input_ref(alias).unwrap().0, "dep");
    assert_eq!(
        lock.resolve_follows_path(&["dep".to_owned()]).unwrap().0,
        "dep"
    );
}

#[test]
fn flake_lock_resolves_nested_follows_paths() {
    let lock = flake_lock(&json!({
        "dep": {"locked": {"type": "github", "owner": "o", "repo": "dep"}},
        "tool": {"inputs": {"dep": ["dep"]}},
        "root": {
            "inputs": {
                "alias": ["middle", "dep"],
                "dep": "dep",
                "middle": ["tool"],
                "tool": "tool",
            },
        },
    }));

    assert_eq!(
        lock.resolve_input_ref(lock.root().unwrap().input("alias").unwrap())
            .unwrap()
            .0,
        "dep"
    );
}

#[test]
fn flake_lock_reports_follows_dead_ends() {
    let missing_input = flake_lock(&json!({
        "root": {"inputs": {"alias": ["missing"]}},
    }));
    assert_eq!(
        missing_input
            .resolve_input_ref(missing_input.root().unwrap().input("alias").unwrap())
            .unwrap_err(),
        FlakeLockError::MissingInput {
            node:  "root".to_owned(),
            input: "missing".to_owned(),
        }
    );

    let missing_node = flake_lock(&json!({
        "root": {"inputs": {"dep": "absent"}},
    }));
    assert_eq!(
        missing_node
            .resolve_input_ref(missing_node.root().unwrap().input("dep").unwrap())
            .unwrap_err(),
        FlakeLockError::MissingNode {
            name: "absent".to_owned(),
        }
    );
}

#[test]
fn flake_lock_reports_follows_cycles_deterministically() {
    let lock = flake_lock(&json!({
        "root": {
            "inputs": {
                "left": ["right"],
                "right": ["left"],
            },
        },
    }));

    assert_eq!(
        lock.resolve_input_ref(lock.root().unwrap().input("left").unwrap())
            .unwrap_err(),
        FlakeLockError::FollowsCycle {
            path: "root.right -> root.left -> root.right".to_owned(),
        }
    );
}
