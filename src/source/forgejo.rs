// SPDX-License-Identifier: EUPL-1.2

use std::fmt::{
    self,
    Display,
};

use serde::{
    Deserialize,
    Serialize,
};

use super::{
    decode_path_segment,
    host,
    parse_query_fields,
    split_query_fragment,
};

/// forgejo and gitea share the same `/api/v1/` surface; this carries which one
/// a pin declared so the fetch layer can skip host detection
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Forgejo,
    Gitea,
}

impl Kind {
    pub const fn scheme(self) -> &'static str {
        match self {
            Self::Forgejo => "forgejo",
            Self::Gitea => "gitea",
        }
    }

    pub fn from_scheme(scheme: &str) -> Option<Self> {
        match scheme {
            "forgejo" => Some(Self::Forgejo),
            "gitea" => Some(Self::Gitea),
            _ => None,
        }
    }
}

impl Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.scheme())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlakeRef {
    pub kind:  Kind,
    pub host:  String,
    pub owner: String,
    pub repo:  String,
    pub reff:  Option<String>,
    pub rev:   Option<String>,
}

/// parse a `forgejo:`/`gitea:` body. host is mandatory: these forges self-host
/// on arbitrary domains with no canonical home
pub fn parse_flake_url(kind: Kind, body: &str) -> Option<FlakeRef> {
    let (path, query) = split_query_fragment(body);
    let mut segs = path.split('/');
    let owner = decode_path_segment(segs.next().filter(|segment| !segment.is_empty())?);
    let repo = decode_path_segment(segs.next().filter(|segment| !segment.is_empty())?);
    let query_fields = parse_query_fields(query);
    let host = host::normalized(query_fields.host?);
    let path_ref = segs.collect::<Vec<_>>().join("/");
    let reff = query_fields
        .reff
        .map(ToOwned::to_owned)
        .or_else(|| (!path_ref.is_empty()).then_some(path_ref));
    Some(FlakeRef {
        kind,
        host,
        owner,
        repo,
        reff,
        rev: query_fields.rev.map(ToOwned::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Kind,
        parse_flake_url,
    };

    #[test]
    fn parses_forgejo_flake_url() {
        let parsed =
            parse_flake_url(Kind::Forgejo, "owner/repo/main?host=Git.Example.Com:8443").unwrap();

        assert_eq!(parsed.kind, Kind::Forgejo);
        assert_eq!(parsed.host, "git.example.com:8443");
        assert_eq!(parsed.owner, "owner");
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.reff.as_deref(), Some("main"));
        assert_eq!(parsed.rev, None);
    }

    #[test]
    fn gitea_alias_parses_with_kind() {
        let parsed = parse_flake_url(Kind::Gitea, "o/r?host=git.example.com&rev=abc").unwrap();

        assert_eq!(parsed.kind, Kind::Gitea);
        assert_eq!(parsed.host, "git.example.com");
        assert_eq!(parsed.rev.as_deref(), Some("abc"));
    }

    #[test]
    fn missing_host_is_rejected() {
        assert!(parse_flake_url(Kind::Forgejo, "o/r/main").is_none());
    }

    #[test]
    fn decodes_nested_group_owner() {
        let parsed =
            parse_flake_url(Kind::Forgejo, "group%2Fsub/repo?host=git.example.com").unwrap();
        assert_eq!(parsed.owner, "group/sub");
    }
}
