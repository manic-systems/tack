// SPDX-License-Identifier: EUPL-1.2

use super::{
    decode_path_segment,
    host,
    parse_query_fields,
    split_query_fragment,
    strip_query_fragment,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoRef {
    pub host:  String,
    pub owner: String,
    pub repo:  String,
}

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
    let clean_url = strip_query_fragment(url);
    if let Some(rest) = clean_url.strip_prefix("ssh://") {
        let (authority, raw_path) = rest.split_once('/')?;
        return parse_git_parts(authority, raw_path, HostPortPolicy::Strip);
    }
    if let Some((authority, raw_path)) = parse_scp_like(clean_url) {
        return parse_git_parts(authority, raw_path, HostPortPolicy::Strip);
    }

    let (default_port, rest) = clean_url
        .strip_prefix("https://")
        .map(|rest| (Some("443"), rest))
        .or_else(|| {
            clean_url
                .strip_prefix("http://")
                .map(|rest| (Some("80"), rest))
        })?;
    let (host, raw_path) = rest.split_once('/')?;
    parse_git_parts(host, raw_path, HostPortPolicy::Default(default_port))
}

pub fn is_default_host(host: &str) -> bool {
    host::normalized(host) == DEFAULT_HOST
}

#[derive(Clone, Copy)]
enum HostPortPolicy {
    Default(Option<&'static str>),
    Strip,
}

fn parse_git_parts(
    authority: &str,
    raw_path: &str,
    port_policy: HostPortPolicy,
) -> Option<RepoRef> {
    let host = host_from_authority(authority, port_policy);
    if !host::is_gitlab(&host) {
        return None;
    }
    let path = raw_path.trim_matches('/');
    if path.is_empty() {
        return None;
    }

    let mut segs = path.split('/').filter(|segment| !segment.is_empty());
    let repo = segs
        .next_back()
        .map(|segment| segment.strip_suffix(".git").unwrap_or(segment))
        .filter(|segment| !segment.is_empty())?;
    let owner = segs.collect::<Vec<_>>().join("/");
    if owner.is_empty() {
        return None;
    }

    Some(RepoRef {
        host,
        owner: decode_path_segment(&owner),
        repo: decode_path_segment(repo),
    })
}

fn parse_scp_like(url: &str) -> Option<(&str, &str)> {
    if url.contains("://") {
        return None;
    }
    let slash = url.find('/').unwrap_or(url.len());
    let colon = url.find(':')?;
    (colon < slash)
        .then(|| url.split_at(colon))
        .map(|(host, path)| (host, path.trim_start_matches(':')))
}

fn host_from_authority(authority: &str, port_policy: HostPortPolicy) -> String {
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    match port_policy {
        HostPortPolicy::Default(default_port) => {
            host::normalized_with_default_port(host, default_port)
        },
        HostPortPolicy::Strip => strip_port(host).to_lowercase(),
    }
}

fn strip_port(host: &str) -> &str {
    let Some((name, port)) = host.rsplit_once(':') else {
        return host;
    };
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return host;
    }
    name
}
