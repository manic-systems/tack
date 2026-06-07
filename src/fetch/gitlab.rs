// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    time::Duration,
};

use serde::Deserialize;

use super::{
    CompareStatus,
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
        let parsed =
            self.http
                .gitlab_json::<GitlabCommit>(&url, host, Some(Duration::from_secs(5)))?;
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

fn merge_base_url(host: &str, owner: &str, repo: &str, old: &str, new: &str) -> String {
    let raw_project = format!("{owner}/{repo}");
    let project = percent_encode(&raw_project);
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

fn percent_encode(value: &str) -> Cow<'_, str> {
    percent_encoding::percent_encode(value.as_bytes(), super::PERCENT_ENCODE_SET).into()
}

#[derive(Deserialize)]
struct GitlabCommit {
    id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        CompareStatus,
        classify,
        merge_base_url,
        percent_encode,
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
}
