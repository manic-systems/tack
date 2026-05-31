// SPDX-License-Identifier: EUPL-1.2

use std::collections::BTreeMap;

use serde::{
    Deserialize,
    Deserializer,
};
use serde_json::Value;

use crate::lock;

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

    pub fn locked_nodes(&self) -> impl Iterator<Item = (&str, &lock::LockedNode)> {
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
    locked: Option<lock::LockedNode>,
}

fn default_root() -> String {
    "root".to_owned()
}

fn deserialize_locked_node<'de, D>(deserializer: D) -> Result<Option<lock::LockedNode>, D::Error>
where
    D: Deserializer<'de>,
{
    let locked = Option::<Value>::deserialize(deserializer)?;
    Ok(locked.and_then(|value| lock::LockedNode::from_value(value).ok()))
}

#[cfg(test)]
mod tests {
    use super::FlakeLock;

    #[test]
    fn locked_nodes_skip_root_and_unknown_locked_nodes() {
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
                (
                    name.to_owned(),
                    node.github().map(|github| github.repo.to_owned()),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(nodes, vec![(
            "nixpkgs".to_owned(),
            Some("nixpkgs".to_owned())
        )]);
    }
}
