// SPDX-License-Identifier: EUPL-1.2

use super::{
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

pub fn normalize_host(host: &str) -> String {
    host::normalized(host)
}

pub fn is_default_host(host: &str) -> bool {
    normalize_host(host) == DEFAULT_HOST
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

pub fn clone_url(host: &str, owner: &str, repo: &str) -> String {
    format!("https://{host}/{owner}/{repo}.git")
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

fn decode_path_segment(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Some(byte) = hex_byte(bytes[index + 1], bytes[index + 2])
        {
            decoded.push(byte);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(decoded).unwrap_or_else(|_| value.to_owned())
}

fn hex_byte(hi: u8, lo: u8) -> Option<u8> {
    Some((hex_nibble(hi)? << 4) | hex_nibble(lo)?)
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_HOST,
        clone_url,
        parse_flake_url,
        parse_git_url,
    };

    #[test]
    fn parses_gitlab_flake_url() {
        let parsed =
            parse_flake_url("Veloren%2Fdev/rfcs/main?host=GitLab.Example.Com:8443").unwrap();

        assert_eq!(parsed.host, "gitlab.example.com:8443");
        assert_eq!(parsed.owner, "Veloren/dev");
        assert_eq!(parsed.repo, "rfcs");
        assert_eq!(parsed.reff.as_deref(), Some("main"));
        assert_eq!(parsed.rev, None);
    }

    #[test]
    fn query_ref_overrides_path_ref() {
        let parsed = parse_flake_url("NixOS/nixpkgs/nixos-unstable?ref=main&rev=abc").unwrap();

        assert_eq!(parsed.host, DEFAULT_HOST);
        assert_eq!(parsed.reff.as_deref(), Some("main"));
        assert_eq!(parsed.rev.as_deref(), Some("abc"));
    }

    #[test]
    fn parses_http_gitlab_url_with_case_and_port() {
        let parsed = parse_git_url("https://GitLab.Example.Com:443/o/r.git?ref=main#frag").unwrap();

        assert_eq!(parsed.host, "gitlab.example.com");
        assert_eq!(parsed.owner, "o");
        assert_eq!(parsed.repo, "r");
    }

    #[test]
    fn parses_http_gitlab_url_preserves_non_default_port() {
        let parsed =
            parse_git_url("https://GitLab.Example.Com:8443/o/r.git?ref=main#frag").unwrap();

        assert_eq!(parsed.host, "gitlab.example.com:8443");
        assert_eq!(parsed.owner, "o");
        assert_eq!(parsed.repo, "r");
    }

    #[test]
    fn parses_http_gitlab_url_normalizes_default_port_by_scheme() {
        let http_default = parse_git_url("http://GitLab.Example.Com:80/o/r.git").unwrap();
        let https_non_default = parse_git_url("https://GitLab.Example.Com:80/o/r.git").unwrap();

        assert_eq!(http_default.host, "gitlab.example.com");
        assert_eq!(https_non_default.host, "gitlab.example.com:80");
    }

    #[test]
    fn parses_http_gitlab_url_after_stripping_query_and_fragment() {
        let parsed = parse_git_url("http://GitLab.Example.Com:443/o/r.git?ref=main#frag").unwrap();

        assert_eq!(parsed.host, "gitlab.example.com:443");
        assert_eq!(parsed.owner, "o");
        assert_eq!(parsed.repo, "r");
    }

    #[test]
    fn parses_ssh_gitlab_url_with_userinfo() {
        let parsed = parse_git_url("ssh://git@GitLab.Example.Com:2222/group/sub/repo.git").unwrap();

        assert_eq!(parsed.host, "gitlab.example.com");
        assert_eq!(parsed.owner, "group/sub");
        assert_eq!(parsed.repo, "repo");
    }

    #[test]
    fn parses_scp_like_gitlab_url() {
        let parsed = parse_git_url("git@GitLab.Example.Com:group/sub/repo.git").unwrap();

        assert_eq!(parsed.host, "gitlab.example.com");
        assert_eq!(parsed.owner, "group/sub");
        assert_eq!(parsed.repo, "repo");
    }

    #[test]
    fn builds_clone_url() {
        assert_eq!(
            clone_url("gitlab.com:8443", "NixOS", "nixpkgs"),
            "https://gitlab.com:8443/NixOS/nixpkgs.git"
        );
    }
}
