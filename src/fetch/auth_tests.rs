// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::HashMap,
    fs,
};

use super::{
    Credential,
    FetchError,
    FetchResult,
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
fn credential_ladder_falls_back_only_for_credential_failures() {
    let mut tried = Vec::new();
    let recovered = run_credentials(Some("t"), "h", true, |credential| {
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
    assert_eq!(recovered.unwrap(), 1_i32);
    assert_eq!(tried, vec!["token", "anon"]);

    let mut count = 0_u8;
    let fatal: FetchResult<i32> = run_credentials(Some("t"), "h", true, |_| {
        count += 1;
        Err(FetchError::Transport("boom".to_owned()))
    });
    assert!(matches!(fatal, Err(FetchError::Transport(_))));
    assert_eq!(count, 1_u8);
}

#[test]
fn access_tokens_scrape_follows_include_and_later_lines_win() {
    let dir = tempfile::tempdir().unwrap();
    let included = dir.path().join("extra.conf");
    fs::write(&included, "access-tokens = gitlab.example.com=inc\n").unwrap();
    let main = dir.path().join("nix.conf");
    fs::write(
        &main,
        "access-tokens = github.com=gh gitlab.com=gl\nextra-access-tokens = \
         gitlab.com=override\n!include extra.conf\n",
    )
    .unwrap();

    let mut tokens = HashMap::new();
    scrape_access_tokens(&main, &mut tokens, 0);

    assert_eq!(tokens.get("github.com").map(String::as_str), Some("gh"));
    assert_eq!(
        tokens.get("gitlab.com").map(String::as_str),
        Some("override")
    );
    assert_eq!(
        tokens.get("gitlab.example.com").map(String::as_str),
        Some("inc")
    );
}
