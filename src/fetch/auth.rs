// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::{
        BTreeSet,
        HashMap,
    },
    env,
    fs,
    mem,
    path::{
        Path,
        PathBuf,
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

/// an env var only when set to a non-empty value; an empty token is no token
fn non_empty_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.is_empty())
}

/// `host = token` pairs scraped from every nix.conf in the standard ladder,
/// merged with later files winning. empty unless opted into (see
/// [`nix_conf_scrape_enabled`])
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

/// scraping another tool's nix.conf for `access-tokens` and replaying them to a
/// forge is invasive, so it is opt-in via `TACK_NIX_CONF_TOKENS` (the NixOS
/// module exposes this as `programs.tack.nixConfTokens`)
fn nix_conf_scrape_enabled() -> bool {
    env_flag_enabled(env::var("TACK_NIX_CONF_TOKENS").ok().as_deref())
}

/// an on/off env flag: present and not an explicit falsey value
fn env_flag_enabled(value: Option<&str>) -> bool {
    matches!(
        value.map(str::to_ascii_lowercase).as_deref(),
        Some(flag) if !matches!(flag, "" | "0" | "false" | "no" | "off")
    )
}

/// nix.conf locations, lowest precedence first: system, then user, then the
/// explicit `NIX_USER_CONF_FILES` override list
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

/// read `access-tokens` / `extra-access-tokens` out of one nix.conf, following
/// `!include` (bounded against include cycles)
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

fn token_warnings() -> &'static Mutex<BTreeSet<String>> {
    static WARNINGS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    WARNINGS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// record a "no token for this host" notice once; surfaced after the spinner
/// finishes so it does not corrupt the live display
pub(super) fn record_token_warning(message: String) {
    token_warnings()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(message);
}

/// drain the deferred token warnings for a command to print after its display
pub fn drain_token_warnings() -> Vec<String> {
    let mut guard = token_warnings()
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
mod tests {
    use std::{
        collections::HashMap,
        fs,
    };

    use super::{
        Credential,
        FetchError,
        FetchResult,
        env_flag_enabled,
        run_credentials,
        scrape_access_tokens,
    };

    fn label(credential: Credential) -> &'static str {
        match credential {
            Credential::Token(_) => "token",
            Credential::Anonymous => "anon",
        }
    }

    #[test]
    fn token_failure_falls_through_to_anon() {
        let mut tried = Vec::new();
        let auth = run_credentials(Some("t"), "h", true, |credential| {
            tried.push(label(credential));
            match credential {
                Credential::Token(_) => {
                    Err(FetchError::Auth {
                        what: "rejected".to_owned(),
                    })
                },
                Credential::Anonymous => Ok(1_i32),
            }
        });
        assert_eq!(auth.unwrap(), 1_i32);
        assert_eq!(tried, vec!["token", "anon"]);

        let limited: FetchResult<i32> = run_credentials(Some("t"), "h", true, |credential| {
            match credential {
                Credential::Token(_) => {
                    Err(FetchError::RateLimited {
                        what: "limited".to_owned(),
                    })
                },
                Credential::Anonymous => Ok(2_i32),
            }
        });
        assert_eq!(limited.unwrap(), 2_i32);
    }

    #[test]
    fn anon_used_directly_when_no_token() {
        let mut tried = Vec::new();
        let result = run_credentials(None, "h", true, |credential| {
            tried.push(label(credential));
            Ok::<_, FetchError>(9_i32)
        });
        assert_eq!(result.unwrap(), 9_i32);
        assert_eq!(tried, vec!["anon"]);
    }

    #[test]
    fn non_credential_token_error_stops_before_anon() {
        let mut count = 0_u8;
        let result: FetchResult<i32> = run_credentials(Some("t"), "h", true, |_| {
            count += 1;
            Err(FetchError::Transport("boom".to_owned()))
        });
        assert!(matches!(result, Err(FetchError::Transport(_))));
        assert_eq!(count, 1_u8);
    }

    #[test]
    fn token_auth_outranks_a_transient_anon_rate_limit() {
        let result: FetchResult<i32> = run_credentials(Some("t"), "h", true, |credential| {
            match credential {
                Credential::Token(_) => {
                    Err(FetchError::Auth {
                        what: "rejected".to_owned(),
                    })
                },
                Credential::Anonymous => {
                    Err(FetchError::RateLimited {
                        what: "limited".to_owned(),
                    })
                },
            }
        });
        assert!(matches!(result, Err(FetchError::Auth { .. })));
    }

    #[test]
    fn no_anon_rung_surfaces_token_auth_or_no_credentials() {
        let rejected: FetchResult<i32> = run_credentials(Some("t"), "h", false, |_| {
            Err(FetchError::Auth {
                what: "rejected".to_owned(),
            })
        });
        assert!(matches!(rejected, Err(FetchError::Auth { .. })));

        let none: FetchResult<i32> = run_credentials(None, "h", false, |_| Ok(1_i32));
        assert!(matches!(none, Err(FetchError::Auth { .. })));
    }

    #[test]
    fn env_flag_is_on_only_for_truthy_values() {
        assert!(env_flag_enabled(Some("1")));
        assert!(env_flag_enabled(Some("true")));
        assert!(env_flag_enabled(Some("YES")));
        assert!(!env_flag_enabled(None));
        assert!(!env_flag_enabled(Some("")));
        assert!(!env_flag_enabled(Some("0")));
        assert!(!env_flag_enabled(Some("false")));
        assert!(!env_flag_enabled(Some("off")));
    }

    #[test]
    fn access_tokens_scrape_follows_include_and_lets_later_lines_win() {
        let dir = tempfile::tempdir().unwrap();
        let included = dir.path().join("extra.conf");
        fs::write(&included, "access-tokens = gitlab.example.com=inc\n").unwrap();
        let main = dir.path().join("nix.conf");
        fs::write(
            &main,
            "# a comment\naccess-tokens = github.com=gh gitlab.com=gl\nextra-access-tokens = \
             gitlab.com=override\n!include extra.conf\n",
        )
        .unwrap();

        let mut tokens = HashMap::new();
        scrape_access_tokens(&main, &mut tokens, 0);

        assert_eq!(tokens.get("github.com").map(String::as_str), Some("gh"));
        // a later line (extra-access-tokens) overrides the earlier value
        assert_eq!(
            tokens.get("gitlab.com").map(String::as_str),
            Some("override")
        );
        // a `!include`d file is followed, relative to the includer
        assert_eq!(
            tokens.get("gitlab.example.com").map(String::as_str),
            Some("inc")
        );
    }
}
