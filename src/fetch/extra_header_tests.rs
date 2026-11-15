// SPDX-License-Identifier: EUPL-1.2

use std::str::FromStr as _;

use gix::config::File;

use super::{
    Reach,
    rules_of,
    select,
};

const NONE: [&str; 0] = [];

fn headers(config: &str, url: &str, reach: Reach) -> Vec<String> {
    let file = File::from_str(config).unwrap();
    select(&rules_of(&file, true), url, reach)
        .iter()
        .map(|header| format!("{}: {}", header.name, header.value.to_str().unwrap()))
        .collect()
}

#[test]
fn matches_git_ranking_of_scoped_and_bare_sections() {
    let config = r#"
[http]
    extraHeader = X-Global: g
[http "https://example.com/"]
    extraHeader = X-Host: h
    extraHeader = X-Two: 2
[http "https://*.com/o"]
    extraHeader = X-Wild: w
[http]
    extraHeader = X-Late: l
"#;
    assert_eq!(
        headers(config, "https://example.com/o/r", Reach::GitTransport),
        ["x-global: g", "x-host: h", "x-two: 2"]
    );
    assert_eq!(headers(config, "https://example.com/o/r", Reach::Scoped), [
        "x-host: h",
        "x-two: 2"
    ]);
    assert_eq!(
        headers(config, "https://other.org/o/r", Reach::GitTransport),
        ["x-global: g", "x-late: l"]
    );
    assert_eq!(
        headers(config, "https://other.org/o/r", Reach::Scoped),
        NONE
    );
}
