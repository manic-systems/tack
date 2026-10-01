// SPDX-License-Identifier: EUPL-1.2

use std::collections::{
    BTreeSet,
    HashMap,
};

use gix::{
    hash::Kind as HashKind,
    objs::{
        self,
        compute_hash,
    },
};
use serde::{
    Deserialize,
    Serialize,
};

use super::{
    CommitObject,
    FetchError,
    FetchResult,
    http::HttpClient,
};
use crate::render::printable;

const COMPARE_PAGE_SIZE: usize = 100;
const MAX_RANGE: usize = 10_000;
const GRAPHQL_BATCH: usize = 100;

/// the commits a github compare lists
pub(super) enum ApiRange {
    Commits(Vec<String>),
    Ancestor,
    Diverged,
    TooLarge,
}

#[derive(Deserialize)]
struct Compare {
    status:        String,
    total_commits: Option<usize>,
    #[serde(default)]
    commits:       Vec<CompareSha>,
}

#[derive(Deserialize)]
struct CompareSha {
    sha: String,
}

pub(super) fn compare_range(
    http: HttpClient,
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
) -> FetchResult<ApiRange> {
    // commit contents are proven by their object hash, but which commits sit
    // between base and head is github's claim, the same trust tack places in it
    // for tarballs
    let mut shas = Vec::new();
    let mut page = 1_usize;
    loop {
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/compare/{base}...{head}?per_page={COMPARE_PAGE_SIZE}&page={page}"
        );
        let (parsed, _) = http.github_json_page::<Compare>(&url, None)?;
        match parsed.status.as_str() {
            "identical" => return Ok(ApiRange::Commits(Vec::new())),
            "behind" => return Ok(ApiRange::Ancestor),
            "diverged" => return Ok(ApiRange::Diverged),
            "ahead" => {},
            other => {
                return Err(FetchError::Github(format!(
                    "compare status {}",
                    printable(other)
                )));
            },
        }
        let total = parsed
            .total_commits
            .ok_or_else(|| FetchError::Github("compare lacks total_commits".to_owned()))?;
        if total > MAX_RANGE {
            return Ok(ApiRange::TooLarge);
        }
        let fresh = parsed.commits.len();
        shas.extend(parsed.commits.into_iter().map(|commit| commit.sha));
        if shas.len() >= total {
            break;
        }
        if fresh == 0 {
            return Err(FetchError::Github(format!(
                "compare lists {} of {total} commits",
                shas.len()
            )));
        }
        page += 1;
    }
    if shas.iter().collect::<BTreeSet<_>>().len() != shas.len() {
        return Err(FetchError::Github(
            "compare lists a commit twice".to_owned(),
        ));
    }
    Ok(ApiRange::Commits(shas))
}

#[derive(Serialize)]
struct Variables {
    owner: String,
    repo:  String,
    #[serde(flatten)]
    oids:  HashMap<String, String>,
}

#[derive(Deserialize)]
struct Data {
    repository: Option<HashMap<String, Option<Node>>>,
}

#[derive(Deserialize)]
struct Node {
    oid:       Option<String>,
    signature: Option<Signature>,
}

#[derive(Deserialize)]
pub(super) struct Signature {
    signature: String,
    payload:   String,
}

/// commits in the order of `oids`, with `None` where github holds no
/// signature for one
pub(super) fn signed_commits(
    http: HttpClient,
    owner: &str,
    repo: &str,
    oids: &[String],
) -> FetchResult<Vec<(String, Option<Signature>)>> {
    let mut found = Vec::with_capacity(oids.len());
    for batch in oids.chunks(GRAPHQL_BATCH) {
        let declared = (0..batch.len())
            .map(|index| format!(", $o{index}: GitObjectID!"))
            .collect::<Vec<_>>()
            .concat();
        let selected = (0..batch.len())
            .map(|index| {
                format!(
                    "c{index}: object(oid: $o{index}) {{ ... on Commit {{ oid signature {{ \
                     signature payload }} }} }} "
                )
            })
            .collect::<Vec<_>>()
            .concat();
        let query = format!(
            "query($owner: String!, $repo: String!{declared}) {{ repository(owner: $owner, name: \
             $repo) {{ {selected}}} }}"
        );
        let variables = Variables {
            owner: owner.to_owned(),
            repo:  repo.to_owned(),
            oids:  batch
                .iter()
                .enumerate()
                .map(|(index, oid)| (format!("o{index}"), oid.clone()))
                .collect(),
        };
        let data = http.github_graphql::<_, Data>(&query, &variables)?;
        let mut nodes = data
            .repository
            .ok_or_else(|| FetchError::Github(format!("repository {owner}/{repo} not visible")))?;
        for (index, oid) in batch.iter().enumerate() {
            let node = nodes
                .remove(&format!("c{index}"))
                .flatten()
                .filter(|node| node.oid.as_deref() == Some(oid.as_str()))
                .ok_or_else(|| {
                    FetchError::Github(format!("commit {} not returned", printable(oid)))
                })?;
            found.push((oid.clone(), node.signature));
        }
    }
    Ok(found)
}

/// rebuilds the raw commit from github's signature and payload, and requires
/// it to hash to the requested id so github can't substitute contents
pub(super) fn commit_object(oid: &str, signed: &Signature) -> FetchResult<CommitObject> {
    let armored = signed.signature.replace('\n', "\n ");
    let data = match signed.payload.split_once("\n\n") {
        Some((headers, message)) => format!("{headers}\ngpgsig {armored}\n\n{message}"),
        None => {
            format!(
                "{}\ngpgsig {armored}\n",
                signed.payload.trim_end_matches('\n')
            )
        },
    }
    .into_bytes();
    let computed = compute_hash(HashKind::Sha1, objs::Kind::Commit, &data)
        .map_err(|err| FetchError::Github(format!("hash commit from github: {err}")))?;
    if computed.to_string() != oid {
        return Err(FetchError::Github(format!(
            "contents for commit {} hash to {computed}",
            printable(oid)
        )));
    }
    Ok(CommitObject {
        id: oid.to_owned(),
        data,
    })
}
