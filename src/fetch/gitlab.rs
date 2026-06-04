// SPDX-License-Identifier: EUPL-1.2

use std::time::Duration;

use serde::Deserialize;

use super::{
    github::{
        BranchComparison,
        CompareStatus,
    },
    http::{
        FetchResult,
        HttpClient,
    },
};

#[derive(Clone, Copy)]
struct GitlabClient {
    http: HttpClient,
}

impl GitlabClient {
    fn global() -> Self {
        Self {
            http: HttpClient::global(),
        }
    }

    fn merge_base(
        self,
        host: &str,
        owner: &str,
        repo: &str,
        old: &str,
        new: &str,
    ) -> FetchResult<Option<CompareStatus>> {
        let url = merge_base_url(host, owner, repo, old, new);
        let parsed: GitlabCommit =
            self.http
                .gitlab_json(&url, host, Some(Duration::from_secs(5)))?;
        Ok(parsed.id.as_deref().map(|base| classify(base, old, new)))
    }
}

/// directional status of head relative to base via the merge-base api, `None`
/// when the api can't classify
pub fn compare_status(
    host: &str,
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
) -> FetchResult<Option<CompareStatus>> {
    GitlabClient::global().merge_base(host, owner, repo, base, head)
}

/// upgrade an un-attempted gitlab comparison to a directional one via the
/// merge-base api; degrade to `unavailable` when the api can't classify
/// (no token for a private project, old gitlab, unrelated histories)
pub(super) fn refine_comparison(
    host: &str,
    owner: &str,
    repo: &str,
    old_rev: Option<&str>,
    new_rev: &str,
    base: BranchComparison,
) -> BranchComparison {
    let Some(old) = old_rev else {
        return base;
    };
    // identical revs, or an already-classified comparison, need no api call
    if old == new_rev || base.status.is_some() {
        return base;
    }
    GitlabClient::global()
        .merge_base(host, owner, repo, old, new_rev)
        .ok()
        .flatten()
        .map_or_else(BranchComparison::unavailable, BranchComparison::verified)
}

pub(super) fn compare_revs(
    host: &str,
    owner: &str,
    repo: &str,
    pinned: Option<&str>,
    old_rev: Option<&str>,
    new_rev: &str,
) -> BranchComparison {
    let Some(old) = old_rev else {
        return BranchComparison::none();
    };
    if old == new_rev {
        return BranchComparison::verified(CompareStatus::Identical);
    }
    if pinned.is_some() {
        return BranchComparison::none();
    }
    refine_comparison(
        host,
        owner,
        repo,
        old_rev,
        new_rev,
        BranchComparison::unavailable(),
    )
}

fn merge_base_url(host: &str, owner: &str, repo: &str, old: &str, new: &str) -> String {
    // the nested-group owner must survive as a single `:id` path segment
    let project = percent_encode(&format!("{owner}/{repo}"));
    let (old_ref, new_ref) = (percent_encode(old), percent_encode(new));
    format!(
        "https://{host}/api/v4/projects/{project}/repository/merge_base?refs[]={old_ref}&refs[]=\
         {new_ref}"
    )
}

/// the merge-base oid of (old, new) decides the direction of new relative to
/// old
fn classify(merge_base: &str, old: &str, new: &str) -> CompareStatus {
    if merge_base == old {
        CompareStatus::Ahead
    } else if merge_base == new {
        CompareStatus::Behind
    } else {
        CompareStatus::Diverged
    }
}

/// rfc 3986 unreserved-set encoding, strict enough for path segments and
/// query values alike
fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut encoded = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            },
            other => {
                encoded.push('%');
                encoded.push(char::from(HEX[usize::from(other / 16)]));
                encoded.push(char::from(HEX[usize::from(other % 16)]));
            },
        }
    }
    encoded
}

#[derive(Deserialize)]
struct GitlabCommit {
    id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        BranchComparison,
        CompareStatus,
        classify,
        merge_base_url,
        percent_encode,
        refine_comparison,
    };

    #[test]
    fn classify_maps_merge_base_to_direction() {
        assert_eq!(classify("old", "old", "new"), CompareStatus::Ahead);
        assert_eq!(classify("new", "old", "new"), CompareStatus::Behind);
        assert_eq!(classify("base", "old", "new"), CompareStatus::Diverged);
    }

    #[test]
    fn percent_encode_escapes_nested_groups() {
        assert_eq!(percent_encode("group/sub/repo"), "group%2Fsub%2Frepo");
        assert_eq!(percent_encode("NixOS/nixpkgs"), "NixOS%2Fnixpkgs");
    }

    #[test]
    fn merge_base_url_targets_v4_api_with_encoded_project_and_refs() {
        assert_eq!(
            merge_base_url("gitlab.example.com:8443", "group/sub", "repo", "OLD", "NEW"),
            "https://gitlab.example.com:8443/api/v4/projects/group%2Fsub%2Frepo/repository/\
             merge_base?refs[]=OLD&refs[]=NEW"
        );
        // a rev is a query value, so reserved characters must not survive raw
        assert_eq!(
            merge_base_url("gitlab.com", "o", "r", "a&b", "c#d"),
            "https://gitlab.com/api/v4/projects/o%2Fr/repository/\
             merge_base?refs[]=a%26b&refs[]=c%23d"
        );
    }

    #[test]
    fn refine_leaves_new_identical_and_classified_comparisons_untouched() {
        // a new pin has no old rev to compare against
        assert_eq!(
            refine_comparison("h", "o", "r", None, "new", BranchComparison::none()),
            BranchComparison::none()
        );
        // identical revs need no api call
        assert_eq!(
            refine_comparison("h", "o", "r", Some("rev"), "rev", BranchComparison::none()),
            BranchComparison::none()
        );
        // an already-classified comparison is left as-is
        assert_eq!(
            refine_comparison(
                "h",
                "o",
                "r",
                Some("old"),
                "new",
                BranchComparison::verified(CompareStatus::Identical)
            ),
            BranchComparison::verified(CompareStatus::Identical)
        );
    }
}
