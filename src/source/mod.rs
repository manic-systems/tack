// SPDX-License-Identifier: EUPL-1.2

pub mod forge;
pub mod git_url;
pub mod id;

pub mod gitlab;
mod host;

use std::{
    borrow::Cow,
    ffi::OsStr,
    path::{
        Component,
        Path,
        PathBuf,
    },
    str::FromStr,
};

use eyre::{
    Result,
    bail,
};

/// fetchable pin source, from an expanded pins.toml url
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Github {
        owner: String,
        repo:  String,
        reff:  Option<String>,
        rev:   Option<String>,
    },
    Git {
        url:  String,
        reff: Option<String>,
        rev:  Option<String>,
    },
    Gitlab {
        host:  String,
        owner: String,
        repo:  String,
        reff:  Option<String>,
        rev:   Option<String>,
    },
    Tarball {
        url: String,
    },
    /// a directory on disk, read at nix eval time. the stored spec is either
    /// absolute or relative to the resolver dir; see
    /// [`localize_path_url_with_warning`]
    Path {
        path: String,
    },
}

pub struct GitTarget<'a> {
    pub url:  Cow<'a, str>,
    pub reff: Option<&'a str>,
    pub rev:  Option<&'a str>,
}

impl Source {
    pub fn git_target(&self) -> Option<GitTarget<'_>> {
        match *self {
            Self::Git {
                ref url,
                ref reff,
                ref rev,
            } => {
                Some(GitTarget {
                    url:  Cow::Borrowed(url),
                    reff: reff.as_deref(),
                    rev:  rev.as_deref(),
                })
            },
            Self::Gitlab {
                ref host,
                ref owner,
                ref repo,
                ref reff,
                ref rev,
            } => {
                Some(GitTarget {
                    url:  Cow::Owned(clone_url(host, owner, repo)),
                    reff: reff.as_deref(),
                    rev:  rev.as_deref(),
                })
            },
            Self::Github { .. } | Self::Tarball { .. } | Self::Path { .. } => None,
        }
    }
}

impl FromStr for Source {
    type Err = eyre::Report;

    fn from_str(expanded: &str) -> Result<Self> {
        if let Some(body) = expanded.strip_prefix("github:") {
            let (path, raw_query) = split_query_fragment(body);
            let fields = parse_query_fields(raw_query);
            let segs = path.split('/').collect::<Vec<&str>>();
            if segs.len() < 2 {
                bail!("malformed github url: {expanded}");
            }
            let reff = fields
                .reff
                .map(ToOwned::to_owned)
                .or_else(|| (segs.len() > 2).then(|| segs[2..].join("/")));
            return Ok(Self::Github {
                owner: segs[0].to_owned(),
                repo: segs[1].to_owned(),
                reff,
                rev: fields.rev.map(ToOwned::to_owned),
            });
        }
        if let Some(body) = expanded.strip_prefix("gitlab:") {
            let Some(parsed) = gitlab::parse_flake_url(body) else {
                bail!("malformed gitlab url: {expanded}");
            };
            return Ok(Self::Gitlab {
                host:  parsed.host,
                owner: parsed.owner,
                repo:  parsed.repo,
                reff:  parsed.reff,
                rev:   parsed.rev,
            });
        }
        if let Some(rest) = expanded.strip_prefix("git+") {
            let (url, raw_query) = split_query_fragment(rest);
            let fields = parse_query_fields(raw_query);
            return Ok(Self::Git {
                url:  url.to_owned(),
                reff: fields.reff.map(ToOwned::to_owned),
                rev:  fields.rev.map(ToOwned::to_owned),
            });
        }
        if let Some(spec) = expanded.strip_prefix("path:") {
            return Ok(Self::Path {
                path: spec.to_owned(),
            });
        }
        if expanded.starts_with("https://") || expanded.starts_with("http://") {
            return Ok(Self::Tarball {
                url: expanded.to_owned(),
            });
        }
        bail!("unsupported url scheme: {expanded}")
    }
}

/// resolve a `path:` url's spec for storage, choosing relative-to-resolver or
/// absolute by where the target sits: relative when it lives within the
/// project's neighbourhood (so it travels with a moved project), absolute when
/// it points somewhere unrelated. non-`path:` urls pass through untouched.
/// warns but does not fail when the target is missing
pub struct LocalizedUrl {
    pub url:     String,
    pub warning: Option<String>,
}

pub fn localize_path_url_with_warning(expanded: &str, tack_dir: &Path) -> LocalizedUrl {
    let Some(spec) = expanded.strip_prefix("path:") else {
        return LocalizedUrl {
            url:     expanded.to_owned(),
            warning: None,
        };
    };
    let (path, warning) = localize_path_spec(spec, tack_dir);
    LocalizedUrl {
        url: format!("path:{path}"),
        warning,
    }
}

fn localize_path_spec(spec: &str, tack_dir: &Path) -> (String, Option<String>) {
    let root = project_root_of(tack_dir);
    let raw = if Path::new(spec).is_absolute() {
        PathBuf::from(spec)
    } else {
        root.join(spec)
    };
    let target = lexical_normalize(&raw);
    let warning =
        (!target.exists()).then(|| format!("path pin target not found: {}", target.display()));
    // "near" the project = inside the directory that holds the project, so a
    // sibling checkout resolves relative; anything further out stays absolute
    let boundary = root.parent().unwrap_or(root.as_path());
    let path = if target.starts_with(boundary) {
        relative_from(tack_dir, &target)
    } else {
        target.to_string_lossy().into_owned()
    };
    (path, warning)
}

/// the project root holding a `.tack` dir, else the dir itself (legacy layout)
fn project_root_of(tack_dir: &Path) -> PathBuf {
    if tack_dir.file_name() == Some(OsStr::new(".tack")) {
        tack_dir
            .parent()
            .map_or_else(|| tack_dir.to_path_buf(), Path::to_path_buf)
    } else {
        tack_dir.to_path_buf()
    }
}

/// fold `.`/`..` out of a path without touching the filesystem
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            },
            Component::CurDir => {},
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                out.push(component.as_os_str());
            },
        }
    }
    out
}

/// relative path from `base` to `target`, both absolute, via `..` hops
fn relative_from(base: &Path, target: &Path) -> String {
    let normalized_base = lexical_normalize(base);
    let mut base_components = normalized_base.components().peekable();
    let mut target_components = target.components().peekable();
    while let (Some(left), Some(right)) = (base_components.peek(), target_components.peek()) {
        if left == right {
            base_components.next();
            target_components.next();
        } else {
            break;
        }
    }
    let mut rel = PathBuf::new();
    for _ in base_components {
        rel.push("..");
    }
    for component in target_components {
        rel.push(component.as_os_str());
    }
    if rel.as_os_str().is_empty() {
        rel.push(".");
    }
    rel.to_string_lossy().into_owned()
}

pub(in crate::source) fn split_query_fragment(value: &str) -> (&str, Option<&str>) {
    let without_fragment = strip_fragment(value);
    without_fragment
        .split_once('?')
        .map_or((without_fragment, None), |(path, query)| {
            (path, Some(query))
        })
}

fn strip_query_fragment(value: &str) -> &str {
    split_query_fragment(value).0
}

fn strip_fragment(value: &str) -> &str {
    value
        .split_once('#')
        .map_or(value, |(without_fragment, _fragment)| without_fragment)
}

#[derive(Default)]
struct QueryFields<'a> {
    pub host: Option<&'a str>,
    pub reff: Option<&'a str>,
    pub rev:  Option<&'a str>,
}

fn parse_query_fields(query: Option<&str>) -> QueryFields<'_> {
    let mut fields = QueryFields::default();
    let Some(raw_query) = query else {
        return fields;
    };
    for kv in raw_query.split('&') {
        let Some((key, value)) = kv.split_once('=') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        match key {
            "host" => fields.host = Some(value),
            "ref" => fields.reff = Some(value),
            "rev" => fields.rev = Some(value),
            _ => {},
        }
    }
    fields
}

/// https clone url for a host/owner/repo on any git forge
pub fn clone_url(host: &str, owner: &str, repo: &str) -> String {
    format!("https://{host}/{owner}/{repo}.git")
}

/// canonicalize a forge host (lowercase, drop the default https port)
pub fn normalize_host(host: &str) -> String {
    host::normalized(host)
}

fn decode_path_segment(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8()
        .map_or_else(|_| value.to_owned(), Into::into)
}

#[cfg(test)]
#[expect(clippy::panic, reason = "panic is the test-failure coping mechanism")]
mod tests {
    use std::path::Path;

    use super::{
        Source,
        clone_url,
        localize_path_url_with_warning,
    };

    #[test]
    fn builds_clone_url() {
        assert_eq!(
            clone_url("gitlab.com:8443", "NixOS", "nixpkgs"),
            "https://gitlab.com:8443/NixOS/nixpkgs.git"
        );
    }

    #[test]
    fn path_url_is_a_path_source() {
        match "path:../sibling".parse::<Source>().unwrap() {
            Source::Path { path } => assert_eq!(path, "../sibling"),
            Source::Github { .. }
            | Source::Git { .. }
            | Source::Gitlab { .. }
            | Source::Tarball { .. } => panic!("expected path source"),
        }
    }

    #[test]
    fn localize_stores_near_paths_relative_and_far_paths_absolute() {
        let tack = Path::new("/home/u/proj/.tack");
        // a sibling checkout lives near the project: relative to the resolver
        assert_eq!(
            localize_path_url_with_warning("path:../sibling", tack).url,
            "path:../../sibling"
        );
        assert_eq!(
            localize_path_url_with_warning("path:./vendor/dep", tack).url,
            "path:../vendor/dep"
        );
        // somewhere unrelated stays absolute so it survives a moved project
        assert_eq!(
            localize_path_url_with_warning("path:/etc/nixos", tack).url,
            "path:/etc/nixos"
        );
        // non-path urls pass through untouched
        assert_eq!(
            localize_path_url_with_warning("github:o/r", tack).url,
            "github:o/r"
        );
    }

    #[test]
    fn git_rev_query() {
        match "git+https://example.com/o/r?ref=main&rev=abc123"
            .parse::<Source>()
            .unwrap()
        {
            Source::Git { url, reff, rev } => {
                assert_eq!(url, "https://example.com/o/r");
                assert_eq!(reff.as_deref(), Some("main"));
                assert_eq!(rev.as_deref(), Some("abc123"));
            },
            Source::Github { .. }
            | Source::Gitlab { .. }
            | Source::Tarball { .. }
            | Source::Path { .. } => {
                panic!("expected git target")
            },
        }
        match "git+ssh://git@example.com/o/r?rev=deadbeef"
            .parse::<Source>()
            .unwrap()
        {
            Source::Git { reff, rev, .. } => {
                assert_eq!(reff, None);
                assert_eq!(rev.as_deref(), Some("deadbeef"));
            },
            Source::Github { .. }
            | Source::Gitlab { .. }
            | Source::Tarball { .. }
            | Source::Path { .. } => {
                panic!("expected git target")
            },
        }
    }

    #[test]
    fn git_query_fragment_is_not_part_of_ref_or_rev() {
        match "git+https://example.com/o/r?ref=main&rev=abc123#frag"
            .parse::<Source>()
            .unwrap()
        {
            Source::Git { url, reff, rev } => {
                assert_eq!(url, "https://example.com/o/r");
                assert_eq!(reff.as_deref(), Some("main"));
                assert_eq!(rev.as_deref(), Some("abc123"));
            },
            Source::Github { .. }
            | Source::Gitlab { .. }
            | Source::Tarball { .. }
            | Source::Path { .. } => {
                panic!("expected git target")
            },
        }
    }

    #[test]
    fn gitlab_url_is_first_class_source() {
        match "gitlab:NixOS/nixpkgs/nixos-unstable?host=GitLab.Example.Com&rev=abc123"
            .parse::<Source>()
            .unwrap()
        {
            Source::Gitlab {
                host,
                owner,
                repo,
                reff,
                rev,
            } => {
                assert_eq!(host, "gitlab.example.com");
                assert_eq!(owner, "NixOS");
                assert_eq!(repo, "nixpkgs");
                assert_eq!(reff.as_deref(), Some("nixos-unstable"));
                assert_eq!(rev.as_deref(), Some("abc123"));
            },
            Source::Github { .. }
            | Source::Git { .. }
            | Source::Tarball { .. }
            | Source::Path { .. } => {
                panic!("expected gitlab target")
            },
        }
    }

    #[test]
    fn git_target_covers_git_and_gitlab_sources() {
        let git = "git+https://example.com/o/r?ref=main&rev=abc123"
            .parse::<Source>()
            .unwrap();
        let git_target = git.git_target().unwrap();
        assert_eq!(git_target.url.as_ref(), "https://example.com/o/r");
        assert_eq!(git_target.reff, Some("main"));
        assert_eq!(git_target.rev, Some("abc123"));

        let gitlab = "gitlab:NixOS/nixpkgs/nixos-unstable?host=gitlab.example.com&rev=abc123"
            .parse::<Source>()
            .unwrap();
        let gitlab_target = gitlab.git_target().unwrap();
        assert_eq!(
            gitlab_target.url.as_ref(),
            "https://gitlab.example.com/NixOS/nixpkgs.git"
        );
        assert_eq!(gitlab_target.reff, Some("nixos-unstable"));
        assert_eq!(gitlab_target.rev, Some("abc123"));
    }

    #[test]
    fn github_rev_is_committish() {
        match "github:o/r?rev=abc123".parse::<Source>().unwrap() {
            Source::Github { reff, rev, .. } => {
                assert_eq!(reff, None);
                assert_eq!(rev.as_deref(), Some("abc123"));
            },
            Source::Git { .. }
            | Source::Gitlab { .. }
            | Source::Tarball { .. }
            | Source::Path { .. } => {
                panic!("expected github target")
            },
        }
    }

    #[test]
    fn https_url_is_tarball() {
        match "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz"
            .parse::<Source>()
            .unwrap()
        {
            Source::Tarball { url } => {
                assert_eq!(
                    url,
                    "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz"
                );
            },
            Source::Github { .. }
            | Source::Git { .. }
            | Source::Gitlab { .. }
            | Source::Path { .. } => {
                panic!("expected tarball target")
            },
        }
        match "http://example.com/release.tar.gz"
            .parse::<Source>()
            .unwrap()
        {
            Source::Tarball { .. } => {},
            Source::Github { .. }
            | Source::Git { .. }
            | Source::Gitlab { .. }
            | Source::Path { .. } => {
                panic!("expected tarball target")
            },
        }
    }
}
