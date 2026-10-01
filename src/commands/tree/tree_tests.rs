// SPDX-License-Identifier: EUPL-1.2

use serde_json::{
    Map,
    Value,
    json,
};

use super::*;

fn github(repo: &str) -> Value {
    json!({"type": "github", "owner": "o", "repo": repo, "rev": "abc"})
}

fn flake_lock(nodes: &Map<String, Value>) -> FlakeLock {
    let lock = json!({"version": 7_u8, "root": "root", "nodes": nodes});
    FlakeLock::parse(&lock.to_string()).unwrap()
}

fn no_follows() -> Follows<'static> {
    Follows {
        first:  BTreeMap::new(),
        deeper: BTreeMap::new(),
    }
}

fn walk(lock: &FlakeLock) -> (Vec<TreeInput>, bool) {
    let mut walk = Walk::new(lock, no_follows());
    let inputs = walk.inputs(lock.root(), 0);
    (inputs, walk.truncated)
}

fn count(inputs: &[TreeInput]) -> usize {
    inputs.iter().map(|input| 1 + count(children(input))).sum()
}

fn children(input: &TreeInput) -> &[TreeInput] {
    if let TreeTarget::Locked { ref inputs, .. } = input.target {
        inputs
    } else {
        &[]
    }
}

#[test]
fn shared_nodes_list_their_inputs_once() {
    const LAYERS: usize = 40;
    let mut nodes = Map::new();
    nodes.insert("root".to_owned(), json!({"inputs": {"a": "n0", "b": "n0"}}));
    for layer in 0..LAYERS {
        let next = format!("n{}", layer + 1);
        let mut node = json!({"locked": github(&format!("r{layer}"))});
        if layer + 1 < LAYERS {
            node["inputs"] = json!({"a": next, "b": next});
        }
        nodes.insert(format!("n{layer}"), node);
    }

    let (inputs, truncated) = walk(&flake_lock(&nodes));

    assert!(!truncated);
    assert_eq!(count(&inputs), 2 * LAYERS);
    assert!(matches!(inputs[1].target, TreeTarget::Repeated(_)));
}

#[test]
fn cycles_stop_at_the_repeat() {
    let mut nodes = Map::new();
    nodes.insert("root".to_owned(), json!({"inputs": {"a": "a"}}));
    nodes.insert(
        "a".to_owned(),
        json!({"locked": github("a"), "inputs": {"b": "b"}}),
    );
    nodes.insert(
        "b".to_owned(),
        json!({"locked": github("b"), "inputs": {"a": "a"}}),
    );

    let (inputs, _) = walk(&flake_lock(&nodes));

    let first = children(&inputs[0]);
    let second = children(&first[0]);
    assert!(matches!(second[0].target, TreeTarget::Repeated(_)));
}

#[test]
fn deep_chains_are_cut_off() {
    let length = MAX_DEPTH * 4;
    let mut nodes = Map::new();
    nodes.insert("root".to_owned(), json!({"inputs": {"next": "n0"}}));
    for link in 0..length {
        let mut node = json!({"locked": github(&format!("r{link}"))});
        if link + 1 < length {
            node["inputs"] = json!({"next": format!("n{}", link + 1)});
        }
        nodes.insert(format!("n{link}"), node);
    }

    let (inputs, truncated) = walk(&flake_lock(&nodes));

    assert!(truncated);
    assert_eq!(count(&inputs), MAX_DEPTH);
}

#[test]
fn unknown_lock_types_stay_visible() {
    let mut nodes = Map::new();
    nodes.insert("root".to_owned(), json!({"inputs": {"src": "src"}}));
    nodes.insert(
        "src".to_owned(),
        json!({"locked": {"type": "sourcehut", "owner": "~o", "repo": "r"}}),
    );

    let (inputs, _) = walk(&flake_lock(&nodes));

    assert!(matches!(inputs[0].target, TreeTarget::Unknown(ref kind) if kind == "sourcehut"));
}

#[test]
fn colliding_follows_keys_resolve_like_the_resolver() {
    let flake_key = "flake:nixpkgs".to_owned();
    let bare_key = "nixpkgs".to_owned();
    let (qualified, bare) = ("a".to_owned(), "b".to_owned());
    let follows = BTreeMap::from([(&bare_key, &bare), (&flake_key, &qualified)]);

    assert_eq!(Follows::flake_side(follows).get("nixpkgs"), Some(&"a"));
}
