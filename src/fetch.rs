// SPDX-License-Identifier: EUPL-1.2

use anyhow::{Context, Result, anyhow, bail};
use git2::build::CheckoutBuilder;
use git2::{Cred, CredentialType, Direction, FetchOptions, RemoteCallbacks, Repository};
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::nar;

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let connector = native_tls::TlsConnector::new().expect("init tls");
        ureq::AgentBuilder::new()
            .tls_connector(Arc::new(connector))
            .build()
    })
}

enum Target {
    Github {
        owner: String,
        repo: String,
        reff: Option<String>,
    },
    Git {
        url: String,
        reff: Option<String>,
        rev: Option<String>,
    },
    Tarball {
        url: String,
    },
}

fn parse(expanded: &str) -> Result<Target> {
    if let Some(body) = expanded.strip_prefix("github:") {
        let (path, reff, rev) = split_query(body);
        let segs: Vec<&str> = path.split('/').collect();
        if segs.len() < 2 {
            bail!("malformed github url: {expanded}");
        }
        let reff = reff
            .or(rev)
            .or_else(|| (segs.len() > 2).then(|| segs[2..].join("/")));
        return Ok(Target::Github {
            owner: segs[0].to_string(),
            repo: segs[1].to_string(),
            reff,
        });
    }
    if let Some(rest) = expanded.strip_prefix("git+") {
        let (url, reff, rev) = split_query(rest);
        return Ok(Target::Git {
            url: url.to_string(),
            reff,
            rev,
        });
    }
    if expanded.starts_with("https://") || expanded.starts_with("http://") {
        return Ok(Target::Tarball {
            url: expanded.to_string(),
        });
    }
    bail!("unsupported url scheme: {expanded}")
}

/// pull out ref= and rev=
fn split_query(s: &str) -> (&str, Option<String>, Option<String>) {
    let Some((path, q)) = s.split_once('?') else {
        return (s, None, None);
    };
    let (mut reff, mut rev) = (None, None);
    for kv in q.split('&') {
        if let Some(v) = kv.strip_prefix("ref=") {
            reff = Some(v.to_string());
        } else if let Some(v) = kv.strip_prefix("rev=") {
            rev = Some(v.to_string());
        }
    }
    (path, reff, rev)
}

/// upstream rev, no tree fetch
pub fn current_rev(expanded: &str) -> Result<String> {
    match parse(expanded)? {
        Target::Github { owner, repo, reff } => {
            let reff = reff.as_deref().unwrap_or("HEAD");
            Ok(gh_commit(&owner, &repo, reff)?.0)
        }
        Target::Git { url, reff, rev } => {
            // a pinned rev never moves; report it without touching the network
            if let Some(rev) = rev {
                return Ok(rev);
            }
            let cb = callbacks();
            let mut remote = git2::Remote::create_detached(url.as_str())?;
            let conn = remote.connect_auth(Direction::Fetch, Some(cb), None)?;
            let want = full_ref(reff.as_deref(), || branch_str(conn.default_branch()));
            for head in conn.list()? {
                if head.name() == want {
                    return Ok(head.oid().to_string());
                }
            }
            bail!("ref {want} not found on {url}")
        }
        Target::Tarball { url } => {
            let resp = agent()
                .request("HEAD", &url)
                .set("User-Agent", "tack")
                .call()
                .or_else(|_| agent().get(&url).set("User-Agent", "tack").call())
                .with_context(|| format!("probe {url}"))?;
            Ok(immutable_url_of(&resp, &url))
        }
    }
}

/// fetch the tree, return (locked node, rev)
pub fn fetch_pin(expanded: &str, submodules: bool) -> Result<(Value, String)> {
    match parse(expanded)? {
        Target::Github { owner, repo, reff } => {
            let reff = reff.as_deref().unwrap_or("HEAD");
            let (rev, last_modified) = gh_commit(&owner, &repo, reff)?;
            let dir = tempfile::tempdir()?;
            let root = download_github_tarball(&owner, &repo, &rev, dir.path())?;
            let nar_hash = nar::hash_path(&root)?;
            let node = json!({
                "type": "github",
                "owner": owner,
                "repo": repo,
                "rev": rev,
                "narHash": nar_hash,
                "lastModified": last_modified,
            });
            Ok((node, rev))
        }
        Target::Git { url, reff, rev } => {
            let dir = tempfile::tempdir()?;
            let (rev, last_modified, refname) = git_checkout(
                &url,
                reff.as_deref(),
                rev.as_deref(),
                submodules,
                dir.path(),
            )?;
            std::fs::remove_dir_all(dir.path().join(".git")).ok();
            let nar_hash = nar::hash_path(dir.path())?;
            let mut node = json!({
                "type": "git",
                "url": url,
                "ref": refname,
                "rev": rev,
                "narHash": nar_hash,
                "lastModified": last_modified,
            });
            if submodules {
                node["submodules"] = json!(true);
            }
            Ok((node, rev))
        }
        Target::Tarball { url } => {
            let resp = agent()
                .get(&url)
                .set("User-Agent", "tack")
                .call()
                .with_context(|| format!("GET {url}"))?;
            let immutable_url = immutable_url_of(&resp, &url);
            let last_modified = resp
                .header("Last-Modified")
                .and_then(|s| epoch_from_http_date(s).ok())
                .unwrap_or(0);
            let format = detect_tar_format(&immutable_url)
                .or_else(|_| detect_tar_format(&url))
                .with_context(|| format!("tarball {url}"))?;

            let dir = tempfile::tempdir()?;
            let root = unpack_tar_stream(resp.into_reader(), format, dir.path())?;
            let nar_hash = nar::hash_path(&root)?;
            let node = json!({
                "type": "tarball",
                "url": immutable_url,
                "narHash": nar_hash,
                "lastModified": last_modified,
            });
            Ok((node, immutable_url))
        }
    }
}

/// Locked URL for a tarball response
fn immutable_url_of(resp: &ureq::Response, fallback: &str) -> String {
    resp.header("Link")
        .and_then(parse_link_immutable)
        .unwrap_or_else(|| {
            let url = resp.get_url();
            if url.is_empty() {
                fallback.to_string()
            } else {
                url.to_string()
            }
        })
}

/// Extract the immutable URL from an HTTP Link header per RFC 8288.
fn parse_link_immutable(header: &str) -> Option<String> {
    for part in header.split(',') {
        let part = part.trim();
        let (url_part, params) = part.split_once(';')?;
        let url = url_part
            .trim()
            .strip_prefix('<')
            .and_then(|s| s.strip_suffix('>'))?;
        for param in params.split(';') {
            let (k, v) = param.trim().split_once('=')?;
            if k.trim().eq_ignore_ascii_case("rel") {
                let v = v.trim().trim_matches('"');
                if v == "immutable" || v == "immutable_link" {
                    return Some(url.to_string());
                }
            }
        }
    }
    None
}

#[derive(Clone, Copy)]
enum TarFormat {
    Gz,
    Xz,
    Plain,
}

fn detect_tar_format(url: &str) -> Result<TarFormat> {
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split('#').next().unwrap_or(path);
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
        Ok(TarFormat::Xz)
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        Ok(TarFormat::Gz)
    } else if lower.ends_with(".tar") {
        Ok(TarFormat::Plain)
    } else {
        bail!(
            "cannot infer tarball format from url (need .tar, .tar.gz/.tgz, or .tar.xz/.txz): {url}"
        )
    }
}

/// Unpack a tarball stream into `into`, strip the single top-level directory
/// and return the stripped root.
fn unpack_tar_stream<R: Read>(reader: R, format: TarFormat, into: &Path) -> Result<PathBuf> {
    let decompressed: Box<dyn Read> = match format {
        TarFormat::Gz => Box::new(flate2::read::GzDecoder::new(reader)),
        TarFormat::Xz => Box::new(xz2::read::XzDecoder::new(reader)),
        TarFormat::Plain => Box::new(reader),
    };
    let mut archive = tar::Archive::new(decompressed);
    archive.set_preserve_permissions(true);
    archive
        .unpack(into)
        .with_context(|| format!("unpack into {}", into.display()))?;
    let mut dirs = std::fs::read_dir(into)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir());
    let root = dirs.next().ok_or_else(|| anyhow!("empty tarball"))?;
    if dirs.next().is_some() {
        bail!("unexpected multiple top-level dirs in tarball");
    }
    Ok(root)
}

fn gh_commit(owner: &str, repo: &str, reff: &str) -> Result<(String, i64)> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{reff}");
    let mut req = agent()
        .get(&url)
        .set("User-Agent", "tack")
        .set("Accept", "application/vnd.github+json");
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    let body = req
        .call()
        .with_context(|| format!("github api {owner}/{repo}@{reff}"))?
        .into_string()?;
    let v: Value = serde_json::from_str(&body)?;
    let rev = v["sha"]
        .as_str()
        .ok_or_else(|| anyhow!("no sha in github response for {owner}/{repo}@{reff}"))?
        .to_string();
    let date = v["commit"]["committer"]["date"]
        .as_str()
        .ok_or_else(|| anyhow!("no commit date for {owner}/{repo}@{reff}"))?;
    Ok((rev, epoch_from_iso(date)?))
}

fn download_github_tarball(owner: &str, repo: &str, rev: &str, into: &Path) -> Result<PathBuf> {
    let url = format!("https://codeload.github.com/{owner}/{repo}/tar.gz/{rev}");
    let reader = agent()
        .get(&url)
        .set("User-Agent", "tack")
        .call()
        .with_context(|| format!("download {url}"))?
        .into_reader();
    let gz = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);
    archive.set_preserve_permissions(true);
    archive.unpack(into)?;
    // codeload wraps everything in one repo-rev/ dir
    let mut dirs = std::fs::read_dir(into)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir());
    let root = dirs
        .next()
        .ok_or_else(|| anyhow!("empty tarball for {owner}/{repo}"))?;
    if dirs.next().is_some() {
        bail!("unexpected multiple top-level dirs in {owner}/{repo} tarball");
    }
    Ok(root)
}

/// check out `rev` (if given) or the tip of `reff` (or remote default) into
/// `into`; return (rev, time, refname)
fn git_checkout(
    url: &str,
    reff: Option<&str>,
    rev: Option<&str>,
    submodules: bool,
    into: &Path,
) -> Result<(String, i64, String)> {
    let repo = Repository::init(into)?;
    let mut remote = repo.remote_anonymous(url)?;

    let refname = {
        let conn = remote.connect_auth(Direction::Fetch, Some(callbacks()), None)?;
        full_ref(reff, || branch_str(conn.default_branch()))
    };

    let mut fo = FetchOptions::new();
    fo.remote_callbacks(callbacks());
    // a specific rev can be anywhere in history, so fetch the ref in full;
    // for a moving ref we only need the tip
    if rev.is_none() {
        fo.depth(1);
    }
    remote
        .fetch(&[&refname], Some(&mut fo), None)
        .with_context(|| format!("fetch {refname} from {url}"))?;

    let commit = match rev {
        Some(rev) => repo
            .revparse_single(rev)
            .with_context(|| format!("rev '{rev}' not reachable from {refname} on {url}"))?
            .peel_to_commit()
            .with_context(|| format!("'{rev}' is not a commit"))?,
        None => repo.find_reference("FETCH_HEAD")?.peel_to_commit()?,
    };
    let rev = commit.id().to_string();
    let time = commit.time().seconds();

    repo.checkout_tree(
        commit.tree()?.as_object(),
        Some(CheckoutBuilder::new().force()),
    )?;
    if submodules {
        update_submodules(&repo)?;
    }
    Ok((rev, time, refname))
}

fn update_submodules(repo: &Repository) -> Result<()> {
    for mut sm in repo.submodules()? {
        let mut opts = git2::SubmoduleUpdateOptions::new();
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(callbacks());
        opts.fetch(fo);
        sm.update(true, Some(&mut opts))?;
    }
    Ok(())
}

fn branch_str(b: Result<git2::Buf, git2::Error>) -> Option<String> {
    b.ok().and_then(|b| b.as_str().map(str::to_string))
}

fn full_ref(reff: Option<&str>, default: impl FnOnce() -> Option<String>) -> String {
    match reff {
        Some(r) if r.starts_with("refs/") => r.to_string(),
        Some(r) => format!("refs/heads/{r}"),
        None => default().unwrap_or_else(|| "HEAD".to_string()),
    }
}

fn callbacks() -> RemoteCallbacks<'static> {
    let mut cb = RemoteCallbacks::new();
    cb.credentials(|_url, username, allowed| {
        let user = username.unwrap_or("git");
        if allowed.contains(CredentialType::SSH_KEY) {
            Cred::ssh_key_from_agent(user)
        } else if allowed.contains(CredentialType::USERNAME) {
            Cred::username(user)
        } else {
            Err(git2::Error::from_str("no supported credential type"))
        }
    });
    cb
}

/// IMF-fixdate (e.g. `Sun, 06 Nov 1994 08:49:37 GMT`) to unix seconds.
fn epoch_from_http_date(s: &str) -> Result<i64> {
    let b = s.as_bytes();
    if b.len() < 29 {
        bail!("bad http date: {s}");
    }
    let n = |r: std::ops::Range<usize>| -> Result<i64> {
        s[r].parse().with_context(|| format!("bad http date: {s}"))
    };
    let day = n(5..7)?;
    let mon = match &s[8..11] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        m => bail!("bad month in http date: {m}"),
    };
    let year = n(12..16)?;
    let hh = n(17..19)?;
    let mi = n(20..22)?;
    let ss = n(23..25)?;
    Ok(days_from_civil(year, mon, day) * 86400 + hh * 3600 + mi * 60 + ss)
}

/// iso8601 to unix seconds
fn epoch_from_iso(s: &str) -> Result<i64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        bail!("bad timestamp: {s}");
    }
    let n = |r: std::ops::Range<usize>| -> Result<i64> {
        s[r].parse().with_context(|| format!("bad timestamp: {s}"))
    };
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mi, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    Ok(days_from_civil(y, m, d) * 86400 + hh * 3600 + mi * 60 + ss)
}

/// days since 1970-01-01 (howard hinnant)
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_rev_query() {
        match parse("git+https://example.com/o/r?ref=main&rev=abc123").unwrap() {
            Target::Git { url, reff, rev } => {
                assert_eq!(url, "https://example.com/o/r");
                assert_eq!(reff.as_deref(), Some("main"));
                assert_eq!(rev.as_deref(), Some("abc123"));
            }
            _ => panic!("expected git target"),
        }
        match parse("git+ssh://git@example.com/o/r?rev=deadbeef").unwrap() {
            Target::Git { reff, rev, .. } => {
                assert_eq!(reff, None);
                assert_eq!(rev.as_deref(), Some("deadbeef"));
            }
            _ => panic!("expected git target"),
        }
    }

    #[test]
    fn github_rev_is_committish() {
        match parse("github:o/r?rev=abc123").unwrap() {
            Target::Github { reff, .. } => assert_eq!(reff.as_deref(), Some("abc123")),
            _ => panic!("expected github target"),
        }
    }

    #[test]
    fn https_url_is_tarball() {
        match parse("https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz").unwrap() {
            Target::Tarball { url } => {
                assert_eq!(
                    url,
                    "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz"
                )
            }
            _ => panic!("expected tarball target"),
        }
        match parse("http://example.com/release.tar.gz").unwrap() {
            Target::Tarball { .. } => {}
            _ => panic!("expected tarball target"),
        }
    }

    #[test]
    fn tar_format_from_extension() {
        assert!(matches!(
            detect_tar_format("https://x/y.tar.xz").unwrap(),
            TarFormat::Xz
        ));
        assert!(matches!(
            detect_tar_format("https://x/y.txz").unwrap(),
            TarFormat::Xz
        ));
        assert!(matches!(
            detect_tar_format("https://x/y.tar.gz").unwrap(),
            TarFormat::Gz
        ));
        assert!(matches!(
            detect_tar_format("https://x/y.tgz").unwrap(),
            TarFormat::Gz
        ));
        assert!(matches!(
            detect_tar_format("https://x/y.tar").unwrap(),
            TarFormat::Plain
        ));
        // querystring and fragment must not defeat detection
        assert!(matches!(
            detect_tar_format("https://x/y.tar.xz?signed=1#frag").unwrap(),
            TarFormat::Xz
        ));
        assert!(detect_tar_format("https://x/y").is_err());
    }

    #[test]
    fn link_header_immutable() {
        let h = "<https://releases.nixos.org/nixos/abc/nixexprs.tar.xz>; rel=\"immutable\"";
        assert_eq!(
            parse_link_immutable(h).as_deref(),
            Some("https://releases.nixos.org/nixos/abc/nixexprs.tar.xz")
        );

        // rel=immutable_link is the historic name used by some nix releases
        let h = "<https://x/y>; rel=\"immutable_link\"";
        assert_eq!(parse_link_immutable(h).as_deref(), Some("https://x/y"));

        // a Link header without an immutable rel yields None, not the wrong URL
        let h = "<https://x/y>; rel=\"canonical\"";
        assert!(parse_link_immutable(h).is_none());

        // multiple values: the immutable one wins regardless of position
        let h = "<https://x/canon>; rel=\"canonical\", <https://x/imm>; rel=\"immutable\"";
        assert_eq!(parse_link_immutable(h).as_deref(), Some("https://x/imm"));
    }

    #[test]
    fn http_date_roundtrip() {
        // 1994-11-06T08:49:37Z = 784111777
        assert_eq!(
            epoch_from_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap(),
            784111777
        );
        assert!(epoch_from_http_date("bogus").is_err());
        assert!(epoch_from_http_date("Sun, 06 Foo 1994 08:49:37 GMT").is_err());
    }

    // our tarball nar hash must equal nix's narHash for this rev
    // cargo test -- --ignored
    #[test]
    #[ignore]
    fn github_narhash_matches_nix() {
        let dir = tempfile::tempdir().unwrap();
        let root = download_github_tarball(
            "bertof",
            "nix-rice",
            "98b16b0f649bb41db9a1c3b32191bccb9a1ec271",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            crate::nar::hash_path(&root).unwrap(),
            "sha256-nt/xmuXaJB/vWlRJ4wpdlYQCIgCzFR6QJwlRyhfNn5o="
        );
    }
}
