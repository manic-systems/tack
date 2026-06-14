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

use eyre::Result;

use crate::error::user_bail;

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
                user_bail!("malformed github url: {expanded}");
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
                user_bail!("malformed gitlab url: {expanded}");
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
        user_bail!("unsupported url scheme: {expanded}")
    }
}

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
    let root = lexical_normalize(&project_root_of(tack_dir));
    let raw = if Path::new(spec).is_absolute() {
        PathBuf::from(spec)
    } else {
        root.join(spec)
    };
    let target = lexical_normalize(&raw);
    let warning =
        (!target.exists()).then(|| format!("path pin target not found: {}", target.display()));
    // external targets stay absolute so `..` cannot escape the store copy
    let path = if target.starts_with(&root) {
        relative_from(tack_dir, &target)
    } else {
        target.to_string_lossy().into_owned()
    };
    (path, warning)
}

fn project_root_of(tack_dir: &Path) -> PathBuf {
    if tack_dir.file_name() == Some(OsStr::new(".tack")) {
        tack_dir
            .parent()
            .map_or_else(|| tack_dir.to_path_buf(), Path::to_path_buf)
    } else {
        tack_dir.to_path_buf()
    }
}

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

pub fn clone_url(host: &str, owner: &str, repo: &str) -> String {
    format!("https://{host}/{owner}/{repo}.git")
}

pub fn normalize_host(host: &str) -> String {
    host::normalized(host)
}

fn decode_path_segment(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8()
        .map_or_else(|_| value.to_owned(), Into::into)
}

#[cfg(test)] mod tests;
