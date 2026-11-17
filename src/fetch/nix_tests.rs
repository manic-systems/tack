// SPDX-License-Identifier: EUPL-1.2

use serde_json::json;

use super::TreeAttrs;

#[test]
fn tree_attrs_use_fetch_tree_names() {
    let attrs = TreeAttrs::Git {
        url:        "https://example.com/repo",
        reff:       "refs/heads/main",
        rev:        "0123456789abcdef0123456789abcdef01234567",
        submodules: false,
        shallow:    true,
    };
    assert_eq!(
        serde_json::to_value(&attrs).unwrap(),
        json!({
            "type": "git",
            "url": "https://example.com/repo",
            "ref": "refs/heads/main",
            "rev": "0123456789abcdef0123456789abcdef01234567",
            "submodules": false,
            "shallow": true,
        })
    );
}
