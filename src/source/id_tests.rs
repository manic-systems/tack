// SPDX-License-Identifier: EUPL-1.2

use serde_json::json;

use super::{
    LockedNode,
    SourceId,
};

fn node(value: serde_json::Value) -> LockedNode {
    LockedNode::from_value(value).unwrap()
}

#[test]
fn source_identity_normalizes_common_url_and_lock_forms() {
    let cases: [(Option<&str>, Option<serde_json::Value>, &str); 5] = [
        (
            Some("github:NixOS/nixpkgs/nixos-unstable"),
            Some(json!({"type": "github", "owner": "NixOS", "repo": "Nixpkgs"})),
            "github:nixos/nixpkgs",
        ),
        (
            Some("gitlab:NixOS/nixpkgs?host=Git.Example.Com"),
            Some(
                json!({"type": "gitlab", "host": "git.example.com", "owner": "NixOS", "repo": "nixpkgs"}),
            ),
            "gitlab:git.example.com/nixos/nixpkgs",
        ),
        (
            Some("git+https://github.com/o/r.git?ref=main"),
            None,
            "github:o/r",
        ),
        (
            Some("git+https://x.com/o/r?ref=main#frag"),
            Some(json!({"type": "git", "url": "https://x.com/o/r?ref=main#frag"})),
            "git+https://x.com/o/r",
        ),
        (
            Some("path:/P/X"),
            Some(json!({"type": "path", "path": "/p/x"})),
            "path:/p/x",
        ),
    ];

    for (url_case, locked_case, expected) in cases {
        if let Some(source_url) = url_case {
            assert_eq!(
                SourceId::from_url(source_url).unwrap().to_string(),
                expected
            );
        }
        if let Some(locked_value) = locked_case {
            assert_eq!(
                SourceId::from_locked(&node(locked_value))
                    .unwrap()
                    .to_string(),
                expected
            );
        }
    }
}

#[test]
fn gitlab_identity_keeps_nested_groups_and_self_hosted_boundaries() {
    let nested = SourceId::from_url("gitlab:group%2Fsub/repo").unwrap();
    let ssh = SourceId::from_url("git+ssh://git@gitlab.com:2222/group/sub/repo.git").unwrap();
    let self_hosted = SourceId::from_locked(&node(
        json!({"type": "gitlab", "host": "Git.Example.Com", "owner": "group/sub", "repo": "repo"}),
    ))
    .unwrap();

    assert_eq!(nested, ssh);
    assert_ne!(nested, self_hosted);
    assert_eq!(
        self_hosted.to_string(),
        "gitlab:git.example.com/group/sub/repo"
    );
}
