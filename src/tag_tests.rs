use std::{
    io::{
        Read as _,
        Write as _,
    },
    net::TcpListener,
    thread,
};

use super::*;

/// one server for the page and the asset it names, answering every request
/// with `body`
fn served(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("free port");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            let mut read = [0_u8; 1];
            let mut head = Vec::new();
            while conn.read(&mut read).unwrap_or(0) == 1 {
                head.push(read[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = conn.write_all(response.as_bytes());
        }
    });
    addr.to_string()
}

#[test]
fn page_tags_rank_fill_and_reach_the_asset() {
    static PAGE: &str = r#"<a href="Linphone-6.2.3-x86_64.AppImage">6.2.3</a>
<a href="Linphone-6.10.0-x86_64.AppImage">6.10.0</a>
<a href="Linphone-latest.AppImage">latest</a>
<a href="Linphone-6.0.0-CallEdition-x86_64.AppImage">CallEdition</a>"#;
    let host = served(PAGE);
    let page = TagPage::new(
        &format!("http://{host}/app/"),
        r#"Linphone-\d[^"]*-x86_64\.AppImage"#,
    )
    .expect("page");

    let followed = follow_asset(
        "linphone",
        &"Linphone-{version}-x86_64.AppImage"
            .parse::<TagTemplate>()
            .expect("template"),
        Some(&page),
        &format!("http://{host}/app/Linphone-{{version}}-x86_64.AppImage"),
    )
    .expect("tagged asset");

    assert_eq!(
        followed.tag.as_deref(),
        Some("Linphone-6.10.0-x86_64.AppImage")
    );
    assert_eq!(
        followed.url,
        format!("http://{host}/app/Linphone-6.10.0-x86_64.AppImage")
    );
}

#[test]
fn a_page_tag_the_template_cannot_read_a_version_from_is_skipped() {
    static PAGE: &str = r#"<a href="Linphone-latest.AppImage">latest</a>
<a href="Linphone-6.2.3.AppImage">6.2.3</a>"#;
    let host = served(PAGE);
    // loose enough that the rolling name matches too, leaving it to the
    // template
    let page = TagPage::new(
        &format!("http://{host}/app/"),
        r#"Linphone-[^"]*\.AppImage"#,
    )
    .expect("page");

    let followed = follow_asset(
        "linphone",
        &"Linphone-{version}.AppImage"
            .parse::<TagTemplate>()
            .expect("template"),
        Some(&page),
        &format!("http://{host}/app/Linphone-{{version}}.AppImage"),
    )
    .expect("tagged asset");

    assert_eq!(followed.tag.as_deref(), Some("Linphone-6.2.3.AppImage"));
}

#[test]
fn a_page_finds_nothing_when_its_regex_matches_no_tag() {
    static PAGE: &str = "<a href=\"Linphone-latest.AppImage\">latest</a>";
    let host = served(PAGE);
    let page = TagPage::new(
        &format!("http://{host}/app/"),
        r#"Linphone-\d[^"]*-x86_64\.AppImage"#,
    )
    .expect("page");

    let found = follow_asset(
        "linphone",
        &"Linphone-{version}-x86_64.AppImage"
            .parse::<TagTemplate>()
            .expect("template"),
        Some(&page),
        &format!("http://{host}/app/Linphone-{{version}}-x86_64.AppImage"),
    );
    assert!(found.is_err_and(|err| err.to_string().contains("serves nothing matching")));
}
