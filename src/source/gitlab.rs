// SPDX-License-Identifier: EUPL-1.2

use super::{
    decode_path_segment,
    git_url,
    host,
    parse_query_fields,
    split_query_fragment,
};

pub const DEFAULT_HOST: &str = "gitlab.com";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlakeRef {
    pub host:  String,
    pub owner: String,
    pub repo:  String,
    pub reff:  Option<String>,
    pub rev:   Option<String>,
}

pub type RepoRef = git_url::RepoRef;

pub fn parse_flake_url(body: &str) -> Option<FlakeRef> {
    let (path, query) = split_query_fragment(body);
    let mut segs = path.split('/');
    let owner = decode_path_segment(segs.next().filter(|segment| !segment.is_empty())?);
    let repo = decode_path_segment(segs.next().filter(|segment| !segment.is_empty())?);
    let query_fields = parse_query_fields(query);
    let path_ref = segs.collect::<Vec<_>>().join("/");
    let reff = query_fields
        .reff
        .map(ToOwned::to_owned)
        .or_else(|| (!path_ref.is_empty()).then_some(path_ref));
    Some(FlakeRef {
        host: query_fields
            .host
            .map_or_else(|| DEFAULT_HOST.to_owned(), host::normalized),
        owner,
        repo,
        reff,
        rev: query_fields.rev.map(ToOwned::to_owned),
    })
}

pub fn parse_git_url(url: &str) -> Option<RepoRef> {
    git_url::parse(url).filter(|repo| host::is_gitlab(&repo.host))
}

pub fn is_default_host(host: &str) -> bool {
    host::normalized(host) == DEFAULT_HOST
}
