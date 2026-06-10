// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    path::{
        Path,
        PathBuf,
    },
    time::Duration,
};

use eyre::{
    Result,
    WrapErr as _,
};
use serde::Deserialize;

use super::{
    CompareStatus,
    FetchResult,
    archive::{
        TarFormat,
        unpack_tar_stream,
    },
    auth::{
        record_fetch_warning,
        with_credential_fallback,
    },
    error::FetchError,
    http::HttpClient,
    time::epoch_from_iso,
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

    fn download_archive(
        self,
        host: &str,
        owner: &str,
        repo: &str,
        rev: &str,
        into: &Path,
    ) -> Result<PathBuf> {
        if owner.contains('/') {
            record_fetch_warning(format!(
                "gitlab subgroup {owner}/{repo} is unsupported by nix's fetchTree; the pin \
                 fetches but will not evaluate"
            ));
        }
        let url = archive_url(host, owner, repo, rev);
        with_credential_fallback(host, true, |credential| {
            let request = HttpClient::with_gitlab_credential(self.http.get(&url), credential);
            let mut resp = request
                .call()
                .map_err(|err| FetchError::from_ureq(err, &url))?;
            if resp.status() != 200 {
                return Err(FetchError::from_response(&mut resp, &url));
            }
            unpack_tar_stream(resp.body_mut().as_reader(), TarFormat::Gz, into)
                .map_err(|err| FetchError::Transport(format!("download {url}: {err}")))
        })
        .map_err(|err| eyre::eyre!("download {url}: {err}"))
    }

    fn commit_last_modified(self, host: &str, owner: &str, repo: &str, rev: &str) -> Option<i64> {
        let raw_project = format!("{owner}/{repo}");
        let project = percent_encode(&raw_project);
        let url = format!("https://{host}/api/v4/projects/{project}/repository/commits/{rev}");
        let commit = self
            .http
            .gitlab_json::<GitlabCommitDate>(&url, host, Some(Duration::from_secs(5)))
            .ok()?;
        epoch_from_rfc3339(commit.committed_date.as_deref()?).ok()
    }
}

pub(super) fn download_archive(
    host: &str,
    owner: &str,
    repo: &str,
    rev: &str,
    into: &Path,
) -> Result<PathBuf> {
    GitlabClient::global().download_archive(host, owner, repo, rev, into)
}

pub(super) fn commit_last_modified(host: &str, owner: &str, repo: &str, rev: &str) -> Option<i64> {
    GitlabClient::global().commit_last_modified(host, owner, repo, rev)
}

fn archive_url(host: &str, owner: &str, repo: &str, rev: &str) -> String {
    let raw_project = format!("{owner}/{repo}");
    let project = percent_encode(&raw_project);
    let sha = percent_encode(rev);
    format!("https://{host}/api/v4/projects/{project}/repository/archive.tar.gz?sha={sha}")
}

/// gitlab's `committed_date` carries the committer's offset; nix checks
/// `lastModified` strictly in UTC, so the offset is subtracted back out.
fn epoch_from_rfc3339(input: &str) -> Result<i64> {
    let wall_clock = epoch_from_iso(input)?;
    Ok(wall_clock - offset_seconds(input)?)
}

fn offset_seconds(input: &str) -> Result<i64> {
    use eyre::{
        ContextCompat as _,
        bail,
    };
    let raw_tail = input
        .get(19..)
        .with_context(|| format!("bad timestamp: {input}"))?;
    let zone = raw_tail.trim_start_matches(|ch: char| ch == '.' || ch.is_ascii_digit());
    if zone == "Z" || zone.is_empty() {
        return Ok(0);
    }
    let (sign, body) = match zone.split_at(1) {
        ("+", rest) => (1, rest),
        ("-", rest) => (-1, rest),
        _ => bail!("bad timezone offset: {input}"),
    };
    let (hh, mm) = body
        .split_once(':')
        .with_context(|| format!("bad timezone offset: {input}"))?;
    let hours: i64 = hh
        .parse()
        .wrap_err_with(|| format!("bad timezone offset: {input}"))?;
    let mins: i64 = mm
        .parse()
        .wrap_err_with(|| format!("bad timezone offset: {input}"))?;
    Ok(sign * (hours * 3_600 + mins * 60))
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

#[derive(Deserialize)]
struct GitlabCommitDate {
    committed_date: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        CompareStatus,
        archive_url,
        classify,
        epoch_from_rfc3339,
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

    #[test]
    fn archive_url_encodes_project_path_and_sha() {
        assert_eq!(
            archive_url("gitlab.com", "group/sub", "repo", "deadbeef"),
            "https://gitlab.com/api/v4/projects/group%2Fsub%2Frepo/repository/archive.tar.gz?\
             sha=deadbeef"
        );
        assert_eq!(
            archive_url("gitlab.com", "interitty", "phpunit", "c3e1924"),
            "https://gitlab.com/api/v4/projects/interitty%2Fphpunit/repository/archive.tar.gz?\
             sha=c3e1924"
        );
    }

    #[test]
    fn rfc3339_offset_is_normalized_to_utc() {
        assert_eq!(
            epoch_from_rfc3339("2024-01-01T12:00:00+01:00").unwrap(),
            epoch_from_rfc3339("2024-01-01T11:00:00Z").unwrap()
        );
        assert_eq!(
            epoch_from_rfc3339("2024-01-01T12:00:00-05:00").unwrap(),
            epoch_from_rfc3339("2024-01-01T17:00:00Z").unwrap()
        );
        assert_eq!(
            epoch_from_rfc3339("2024-01-01T00:00:00Z").unwrap(),
            1_704_067_200
        );
        assert_eq!(
            epoch_from_rfc3339("2024-01-01T12:00:00.123+01:00").unwrap(),
            epoch_from_rfc3339("2024-01-01T11:00:00Z").unwrap()
        );
    }

    #[test]
    #[ignore = "hits gitlab.com"]
    fn gitlab_narhash_matches_nix() {
        use crate::nar;

        let dir = tempfile::tempdir().unwrap();
        let root = super::download_archive(
            "gitlab.com",
            "interitty",
            "phpunit",
            "c3e19245295fc118aa0abdb8a3cbf68e75d3e16b",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            nar::hash_path(&root).unwrap(),
            "sha256-RCiTYvloZmYks4/7FkhLv/JogtcAV1TCAJqTMLLwHJg="
        );
    }
}
