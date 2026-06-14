// SPDX-License-Identifier: EUPL-1.2

use super::{
    CompareStatus,
    GithubRefCompareData,
};

#[test]
fn parses_graphql_ref_compare_response() {
    let parsed = serde_json::from_str::<GithubRefCompareData>(
        r#"{
            "repository": {
                "targetRef": {
                    "target": {
                        "oid": "new",
                        "committedDate": "2026-05-30T18:08:13Z"
                    },
                    "compare": {
                        "status": "BEHIND",
                        "aheadBy": 0,
                        "behindBy": 1264
                    }
                }
            }
        }"#,
    )
    .unwrap();

    let resolved = parsed.resolve().unwrap();

    assert_eq!(resolved.rev, "new");
    assert_eq!(resolved.status, Some(CompareStatus::Ahead));
}
