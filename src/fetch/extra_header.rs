// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::BTreeMap,
    path::{
        Path,
        PathBuf,
    },
    sync::{
        Arc,
        Mutex,
        PoisonError,
    },
};

use gix::{
    bstr::{
        BStr,
        ByteSlice as _,
    },
    config::{
        File,
        section::is_trusted,
    },
    sec::Trust,
};
use ureq::{
    RequestBuilder,
    http::{
        HeaderName,
        HeaderValue,
        header::AUTHORIZATION,
    },
};

use super::auth::record_fetch_warning;

static PROJECT_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static RULES: Mutex<BTreeMap<Option<PathBuf>, Arc<[Rule]>>> = Mutex::new(BTreeMap::new());

/// `dir` selects `includeIf.gitdir` files, which actions/checkout relies on
pub fn read_git_config_of(dir: &Path) {
    *PROJECT_DIR.lock().unwrap_or_else(PoisonError::into_inner) = Some(dir.to_owned());
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Reach {
    /// also takes bare `[http]` values
    GitTransport,
    Scoped,
}

#[derive(Clone, Debug)]
pub(super) struct ExtraHeader {
    name:  HeaderName,
    value: HeaderValue,
}

impl ExtraHeader {
    pub(super) fn is_authorization(&self) -> bool {
        self.name == AUTHORIZATION
    }
}

pub(super) fn for_url(url: &str, reach: Reach) -> Vec<ExtraHeader> {
    let dir = PROJECT_DIR
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let rules = Arc::clone(
        RULES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(dir.clone())
            .or_insert_with(|| load(dir.as_deref()).into()),
    );
    select(&rules, url, reach)
}

/// a configured `Authorization` is skipped when tack sets its own, since CI
/// auth is scoped to the checked out repo
pub(super) fn apply<B>(
    mut request: RequestBuilder<B>,
    headers: &[ExtraHeader],
) -> RequestBuilder<B> {
    if let Some(existing) = request.headers_mut() {
        let has_own_auth = existing.contains_key(AUTHORIZATION);
        let added = headers
            .iter()
            .filter(|header| !(has_own_auth && header.is_authorization()))
            .collect::<Vec<_>>();
        for header in &added {
            existing.remove(&header.name);
        }
        for header in added {
            existing.append(header.name.clone(), header.value.clone());
        }
    }
    request
}

#[derive(Debug)]
struct Rule {
    pattern: Option<Pattern>,
    value:   Value,
}

#[derive(Debug)]
enum Value {
    Header(ExtraHeader),
    Reset,
}

fn select(rules: &[Rule], url: &str, reach: Reach) -> Vec<ExtraHeader> {
    let Some(target) = Target::parse_target(url) else {
        return Vec::new();
    };
    let mut best = None;
    let mut picked = Vec::new();
    for rule in rules {
        let specificity = match rule.pattern {
            None => Specificity::default(),
            Some(ref pattern) => {
                let Some(found) = pattern.specificity_for(&target) else {
                    continue;
                };
                found
            },
        };
        if best.is_some_and(|seen| specificity < seen) {
            continue;
        }
        best = Some(specificity);
        match rule.value {
            Value::Header(ref header) => picked.push((rule.pattern.is_some(), header)),
            Value::Reset => picked.clear(),
        }
    }
    picked
        .into_iter()
        .filter(|&(scoped, _)| scoped || reach == Reach::GitTransport)
        .map(|(_, header)| header.clone())
        .collect()
}

fn load(project_dir: Option<&Path>) -> Vec<Rule> {
    let start = project_dir.and_then(|dir| dir.ancestors().find(|path| path.is_dir()));
    if let Some(dir) = start {
        match gix::discover(dir) {
            Ok(repo) => {
                // gix holds includes from outside safe.directory in reduced
                // trust, where git does not
                let admitted = repo.git_dir_trust() == Trust::Full;
                return rules_of(repo.config_snapshot().plumbing(), admitted);
            },
            Err(err) if dir.ancestors().any(|path| path.join(".git").exists()) => {
                record_fetch_warning(format!(
                    "reading http.extraHeader from the git repository at {}: {err}",
                    dir.display()
                ));
            },
            Err(_) => {},
        }
    }
    let globals = File::from_globals().and_then(|mut globals| {
        globals.append(File::from_environment_overrides()?)?;
        Ok(globals)
    });
    match globals {
        Ok(config) => rules_of(&config, true),
        Err(err) => {
            record_fetch_warning(format!("reading http.extraHeader from git config: {err}"));
            Vec::new()
        },
    }
}

fn rules_of(config: &File, repo_admitted: bool) -> Vec<Rule> {
    let Some(sections) = config.sections_by_name("http") else {
        return Vec::new();
    };
    let mut rules = Vec::new();
    for section in sections.filter(|section| repo_admitted || is_trusted(section.meta())) {
        let values = section.values("extraHeader");
        if values.is_empty() {
            continue;
        }
        let pattern = match section.header().subsection_name() {
            None => None,
            Some(raw) => {
                let parsed = raw.to_str().ok().and_then(Pattern::parse);
                if parsed.is_none() {
                    record_fetch_warning(
                        "ignoring a git config [http \"<url>\"] section whose url tack can't match"
                            .to_owned(),
                    );
                    continue;
                }
                parsed
            },
        };
        for value in values {
            if let Some(parsed) = parse_value(value.as_bstr()) {
                rules.push(Rule {
                    pattern: pattern.clone(),
                    value:   parsed,
                });
            }
        }
    }
    rules
}

fn parse_value(raw: &BStr) -> Option<Value> {
    if raw.is_empty() {
        return Some(Value::Reset);
    }
    let parsed = raw.to_str().ok().and_then(|line| {
        let (name, value) = line.split_once(':')?;
        let mut sensitive = HeaderValue::from_str(value.trim())
            .ok()
            .filter(|parsed| !parsed.is_empty())?;
        sensitive.set_sensitive(true);
        Some(ExtraHeader {
            name:  HeaderName::from_bytes(name.trim().as_bytes()).ok()?,
            value: sensitive,
        })
    });
    if parsed.is_none() {
        record_fetch_warning(
            "ignoring an http.extraHeader in git config that is not a `Name: value` header"
                .to_owned(),
        );
    }
    parsed.map(Value::Header)
}

/// field order is git's urlmatch ranking
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Specificity {
    host_len:     usize,
    path_len:     usize,
    user_matched: bool,
}

#[derive(Clone, Debug)]
struct Pattern {
    scheme: String,
    user:   Option<String>,
    host:   String,
    port:   Option<u16>,
    path:   String,
}

impl Pattern {
    fn parse(raw: &str) -> Option<Self> {
        if raw.contains(['?', '#']) {
            return None;
        }
        let parts = UrlParts::parse(raw)?;
        Some(Self {
            path:   parts
                .path
                .strip_suffix('/')
                .unwrap_or(&parts.path)
                .to_owned(),
            scheme: parts.scheme,
            user:   parts.user,
            host:   parts.host,
            port:   parts.port,
        })
    }

    fn specificity_for(&self, target: &Target) -> Option<Specificity> {
        let user_matched = match self.user {
            Some(ref user) if target.user.as_ref() != Some(user) => return None,
            Some(_) => true,
            None => false,
        };
        let path_matches = target
            .path
            .strip_prefix(&self.path)
            .is_some_and(|rest| self.path.is_empty() || rest.is_empty() || rest.starts_with('/'));
        (self.scheme == target.scheme
            && self.port == target.port
            && host_matches(&self.host, &target.host)
            && path_matches)
            .then_some(Specificity {
                host_len: self.host.len(),
                path_len: self.path.len(),
                user_matched,
            })
    }
}

type Target = UrlParts;

#[derive(Clone, Debug, PartialEq, Eq)]
struct UrlParts {
    scheme: String,
    user:   Option<String>,
    host:   String,
    port:   Option<u16>,
    path:   String,
}

impl UrlParts {
    /// git keeps the query on the last segment, so `/a?q` is not under `/a`
    fn parse_target(raw: &str) -> Option<Self> {
        let (before, query) = raw
            .find(['?', '#'])
            .map_or((raw, ""), |mark| raw.split_at(mark));
        let mut parts = Self::parse(before)?;
        if parts.host.contains('*') {
            return None;
        }
        normalize_escapes(query)?;
        parts.path.push_str(query);
        Some(parts)
    }

    fn parse(raw: &str) -> Option<Self> {
        let (raw_scheme, rest) = raw.split_once("://")?;
        let scheme = raw_scheme.to_ascii_lowercase();
        let (authority, path) = rest
            .find('/')
            .map_or((rest, "/"), |slash| rest.split_at(slash));
        let (user, host_port) = match authority.split_once('@') {
            None => (None, authority),
            Some((_, host_port)) if host_port.contains('@') => return None,
            Some((userinfo, host_port)) => {
                let user = userinfo.split_once(':').map_or(userinfo, |(user, _)| user);
                (Some(normalize_escapes(user)?), host_port)
            },
        };
        let (host, port_text) = if let Some(bracketed) = host_port.strip_prefix('[') {
            let (literal, after) = bracketed.split_once(']')?;
            if !after.is_empty() && !after.starts_with(':')
                || !literal
                    .chars()
                    .all(|ch| ch.is_ascii_hexdigit() || ch == ':' || ch == '.')
            {
                return None;
            }
            (host_port.get(..=literal.len())?, after.strip_prefix(':'))
        } else {
            let (name, port) = host_port
                .rsplit_once(':')
                .map_or((host_port, None), |(name, port)| (name, Some(port)));
            let bare = name.strip_suffix('.').unwrap_or(name);
            let valid = bare
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '*'));
            (valid.then_some(bare)?, port)
        };
        let port = match port_text.filter(|text| !text.is_empty()) {
            None => None,
            Some(text) if text.bytes().all(|byte| byte.is_ascii_digit()) => {
                Some(text.parse::<u16>().ok().filter(|port| *port != 0)?)
            },
            Some(_) => return None,
        };
        if host.is_empty() {
            return None;
        }
        let default_port = match scheme.as_str() {
            "http" => Some(80),
            "https" => Some(443),
            _ => None,
        };
        Some(Self {
            port: port.filter(|given| Some(*given) != default_port),
            scheme,
            user,
            host: host.to_ascii_lowercase(),
            path: normalize_path(path)?,
        })
    }
}

fn normalize_path(path: &str) -> Option<String> {
    let mut segments: Vec<String> = Vec::new();
    for raw in path.split('/').skip(1) {
        let segment = normalize_escapes(raw)?;
        match segment.as_str() {
            "." => {},
            ".." => {
                segments.pop()?;
            },
            _ => segments.push(segment),
        }
    }
    Some(format!("/{}", segments.join("/")))
}

/// git's `append_normalized_escapes`
fn normalize_escapes(raw: &str) -> Option<String> {
    const UNSAFE: &[u8] = b" <>\"%{}|\\^`";
    const RESERVED: &[u8] = b":/?#[]@!$&'()*+,;=";
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut bytes = raw.bytes();
    let mut normalized = String::with_capacity(raw.len());
    while let Some(byte) = bytes.next() {
        let (decoded, was_escaped) = if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            (u8::try_from(high * 16 + low).ok()?, true)
        } else {
            (byte, false)
        };
        if decoded <= 0x1F
            || decoded >= 0x7F
            || UNSAFE.contains(&decoded)
            || (was_escaped && RESERVED.contains(&decoded))
        {
            normalized.push('%');
            for nibble in [decoded >> 4_u8, decoded & 0xF_u8] {
                normalized.extend(HEX.get(usize::from(nibble)).copied().map(char::from));
            }
        } else {
            normalized.push(char::from(decoded));
        }
    }
    Some(normalized)
}

fn host_matches(pattern: &str, host: &str) -> bool {
    let wanted = pattern.split('.');
    let given = host.split('.');
    wanted.clone().count() == given.clone().count()
        && wanted
            .zip(given)
            .all(|(want, label)| want == "*" || want == label)
}

#[cfg(test)]
#[path = "extra_header_tests.rs"]
mod tests;
