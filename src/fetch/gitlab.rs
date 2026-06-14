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

/// nix expects utc not gitlab's offset wall clock
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
