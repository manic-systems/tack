// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::{
        BTreeSet,
        HashMap,
    },
    env,
    fmt,
    fs,
    mem,
    path::{
        Path,
        PathBuf,
    },
    process::{
        Command,
        Stdio,
    },
    sync::{
        Mutex,
        OnceLock,
        PoisonError,
    },
};

use etcetera::BaseStrategy as _;

use super::error::{
    FetchError,
    FetchResult,
};

pub(super) fn token_for_host(host: &str) -> Option<&'static str> {
    let env_token = match host {
        "github.com" => github_env_token(),
        "gitlab.com" => gitlab_env_token(),
        _ => None,
    };
    env_token.or_else(|| nix_conf_tokens().get(host).map(String::as_str))
}

fn github_env_token() -> Option<&'static str> {
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN
        .get_or_init(|| non_empty_env("GITHUB_TOKEN").or_else(|| non_empty_env("GH_TOKEN")))
        .as_deref()
}

fn gitlab_env_token() -> Option<&'static str> {
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN
        .get_or_init(|| non_empty_env("GITLAB_TOKEN"))
        .as_deref()
}

fn non_empty_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.is_empty())
}

fn nix_conf_tokens() -> &'static HashMap<String, String> {
    static TOKENS: OnceLock<HashMap<String, String>> = OnceLock::new();
    TOKENS.get_or_init(|| {
        let mut tokens = HashMap::new();
        if nix_conf_scrape_enabled() {
            for file in nix_conf_files() {
                scrape_access_tokens(&file, &mut tokens, 0);
            }
        }
        tokens
    })
}

fn nix_conf_scrape_enabled() -> bool {
    env_flag_enabled(env::var("TACK_NIX_CONF_TOKENS").ok().as_deref())
}

fn env_flag_enabled(value: Option<&str>) -> bool {
    matches!(
        value.map(str::to_ascii_lowercase).as_deref(),
        Some(flag) if !matches!(flag, "" | "0" | "false" | "no" | "off")
    )
}

fn nix_conf_files() -> Vec<PathBuf> {
    let mut files = vec![PathBuf::from("/etc/nix/nix.conf")];
    if let Ok(strategy) = etcetera::choose_base_strategy() {
        files.push(strategy.config_dir().join("nix/nix.conf"));
    }
    if let Some(list) = env::var_os("NIX_USER_CONF_FILES") {
        files.extend(env::split_paths(&list));
    }
    files
}

fn scrape_access_tokens(path: &Path, tokens: &mut HashMap<String, String>, depth: u8) {
    const MAX_INCLUDE_DEPTH: u8 = 16;

    if depth > MAX_INCLUDE_DEPTH {
        return;
    }
    let Ok(contents) = fs::read_to_string(path) else {
        return;
    };
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("!include") {
            let target = rest.trim_start_matches('?').trim();
            if !target.is_empty() {
                let included = path
                    .parent()
                    .filter(|_| !Path::new(target).is_absolute())
                    .map_or_else(|| PathBuf::from(target), |base| base.join(target));
                scrape_access_tokens(&included, tokens, depth + 1);
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if matches!(key.trim(), "access-tokens" | "extra-access-tokens") {
            for pair in value.split_whitespace() {
                if let Some((host, token)) = pair.split_once('=')
                    && !host.is_empty()
                    && !token.is_empty()
                {
                    tokens.insert(host.to_owned(), token.to_owned());
                }
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct HttpCredential {
    username:      String,
    authorization: String,
}

impl HttpCredential {
    fn new(username: String, secret: &str) -> Self {
        let raw = format!("{username}:{secret}");
        let authorization = format!("Basic {}", data_encoding::BASE64.encode(raw.as_bytes()));
        Self {
            username,
            authorization,
        }
    }

    pub(super) fn authorization(&self) -> &str {
        &self.authorization
    }
}

impl fmt::Debug for HttpCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpCredential")
            .field("username", &self.username)
            .field("secret", &"<redacted>")
            .finish_non_exhaustive()
    }
}

fn http_credentials() -> &'static Mutex<HashMap<String, Option<HttpCredential>>> {
    static CREDENTIALS: OnceLock<Mutex<HashMap<String, Option<HttpCredential>>>> = OnceLock::new();
    CREDENTIALS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn resolve_http_credential(host: &str) -> Option<HttpCredential> {
    let mut guard = http_credentials()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(cached) = guard.get(host) {
        return cached.clone();
    }
    let resolved = ladder_credential(host);
    guard.insert(host.to_owned(), resolved.clone());
    resolved
}

fn ladder_credential(host: &str) -> Option<HttpCredential> {
    #[cfg(test)]
    if let Some(seeded) = test_credential_override(host) {
        return Some(seeded);
    }
    token_for_host(host)
        .map(|token| HttpCredential::new("oauth2".to_owned(), token))
        .or_else(|| netrc_credential(host))
        .or_else(|| git_helper_credential(host))
}

#[cfg(test)]
fn test_credential_override(host: &str) -> Option<HttpCredential> {
    test_credentials()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(host)
        .cloned()
}

#[cfg(test)]
fn test_credentials() -> &'static Mutex<HashMap<String, HttpCredential>> {
    static OVERRIDES: OnceLock<Mutex<HashMap<String, HttpCredential>>> = OnceLock::new();
    OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(super) fn seed_resolvable_credential(host: &str, username: &str, secret: &str) {
    test_credentials()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            host.to_owned(),
            HttpCredential::new(username.to_owned(), secret),
        );
}

pub(super) fn cached_http_credential(host: &str) -> Option<HttpCredential> {
    http_credentials()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(host)
        .cloned()
        .flatten()
}

fn netrc_credential(host: &str) -> Option<HttpCredential> {
    let path = etcetera::home_dir().ok()?.join(".netrc");
    let contents = fs::read_to_string(path).ok()?;
    parse_netrc(&contents, host)
}

fn parse_netrc(contents: &str, host: &str) -> Option<HttpCredential> {
    let mut tokens = netrc_tokens(contents).into_iter();
    let mut in_machine = false;
    let mut login: Option<String> = None;
    let mut password: Option<String> = None;
    while let Some(token) = tokens.next() {
        match token.as_str() {
            "machine" => {
                in_machine = tokens.next().is_some_and(|name| name == host);
            },
            "default" => in_machine = false,
            "login" if in_machine => login = tokens.next(),
            "password" if in_machine => password = tokens.next(),
            _ => {},
        }
        if in_machine && login.is_some() && password.is_some() {
            break;
        }
    }
    Some(HttpCredential::new(login?, &password?))
}

fn netrc_tokens(contents: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut lines = contents.lines().peekable();
    while let Some(line) = lines.next() {
        for word in line.split_whitespace() {
            if word == "macdef" {
                while lines.peek().is_some_and(|next| !next.trim().is_empty()) {
                    lines.next();
                }
                lines.next();
                break;
            }
            tokens.push(word.to_owned());
        }
    }
    tokens
}

fn git_helper_credential(host: &str) -> Option<HttpCredential> {
    let mut child = Command::new("git")
        .args(["credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    {
        use std::io::Write as _;

        let mut stdin = child.stdin.take()?;
        write!(stdin, "protocol=https\nhost={host}\n\n").ok()?;
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut username = None;
    let mut password = None;
    for line in stdout.lines() {
        if let Some(value) = line.strip_prefix("username=") {
            username = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("password=") {
            password = Some(value.to_owned());
        }
    }
    Some(HttpCredential::new(username?, &password?))
}

fn fetch_warnings() -> &'static Mutex<BTreeSet<String>> {
    static WARNINGS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    WARNINGS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

pub(super) fn record_fetch_warning(message: String) {
    fetch_warnings()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(message);
}

pub fn drain_fetch_warnings() -> Vec<String> {
    let mut guard = fetch_warnings()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    mem::take(&mut *guard).into_iter().collect()
}

#[derive(Clone, Copy)]
pub(super) enum Credential {
    Token(&'static str),
    Anonymous,
}

pub(super) fn with_credential_fallback<T>(
    host: &str,
    allow_anon: bool,
    attempt: impl FnMut(Credential) -> FetchResult<T>,
) -> FetchResult<T> {
    run_credentials(token_for_host(host), host, allow_anon, attempt)
}

fn run_credentials<T>(
    token: Option<&'static str>,
    host: &str,
    allow_anon: bool,
    mut attempt: impl FnMut(Credential) -> FetchResult<T>,
) -> FetchResult<T> {
    let token_failure = match token {
        Some(value) => {
            match attempt(Credential::Token(value)) {
                Err(err) if err.is_credential_failure() => Some(err),
                result => return result,
            }
        },
        None => None,
    };
    if !allow_anon {
        return Err(token_failure.unwrap_or_else(|| {
            FetchError::Auth {
                what: format!("{host}: no usable credentials"),
            }
        }));
    }
    match attempt(Credential::Anonymous) {
        Err(anon_failure) => Err(most_actionable(token_failure, anon_failure)),
        result => result,
    }
}

fn most_actionable(token_failure: Option<FetchError>, anon_failure: FetchError) -> FetchError {
    match token_failure {
        Some(failure @ FetchError::Auth { .. }) => failure,
        _ => anon_failure,
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
