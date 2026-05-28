// SPDX-License-Identifier: EUPL-1.2

use std::{
    env,
    fs,
    io::Read,
    ops::Range,
    path::{
        Path,
        PathBuf,
    },
    sync::OnceLock,
};

use anyhow::{
    Context as _,
    Result,
    anyhow,
    bail,
};
use flate2::read::GzDecoder;
use git2::{
    Cred,
    CredentialType,
    Direction,
    FetchOptions,
    RemoteCallbacks,
    Repository,
    build::CheckoutBuilder,
};
use serde_json::{
    Value,
    json,
};
use ureq::{
    Agent,
    Body,
    ResponseExt as _,
    http,
    tls::{
        TlsConfig,
        TlsProvider,
    },
};
use xz2::read::XzDecoder;

use crate::{
    nar,
    pins::Unpack,
};

fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let config = TlsConfig::builder()
            .provider(TlsProvider::NativeTls)
            .build();
        Agent::config_builder().tls_config(config).build().into()
    })
}

enum Target {
    Github {
        owner: String,
        repo:  String,
        reff:  Option<String>,
    },
    Git {
        url:  String,
        reff: Option<String>,
        rev:  Option<String>,
    },
    Tarball {
        url: String,
    },
}

fn parse(expanded: &str) -> Result<Target> {
    if let Some(body) = expanded.strip_prefix("github:") {
        let (path, query_ref, query_sha) = split_query(body);
        let segs = path.split('/').collect::<Vec<&str>>();
        if segs.len() < 2 {
            bail!("malformed github url: {expanded}");
        }
        let reff = query_ref
            .or(query_sha)
            .or_else(|| (segs.len() > 2).then(|| segs[2..].join("/")));
        return Ok(Target::Github {
            owner: segs[0].to_owned(),
            repo: segs[1].to_owned(),
            reff,
        });
    }
    if let Some(rest) = expanded.strip_prefix("git+") {
        let (url, reff, rev) = split_query(rest);
        return Ok(Target::Git {
            url: url.to_owned(),
            reff,
            rev,
        });
    }
    if expanded.starts_with("https://") || expanded.starts_with("http://") {
        return Ok(Target::Tarball {
            url: expanded.to_owned(),
        });
    }
    bail!("unsupported url scheme: {expanded}")
}

/// pull out ref= and rev=
fn split_query(str: &str) -> (&str, Option<String>, Option<String>) {
    let Some((path, query)) = str.split_once('?') else {
        return (str, None, None);
    };
    let (mut reff, mut rev) = (None, None);
    for kv in query.split('&') {
        if let Some(value) = kv.strip_prefix("ref=") {
            reff = Some(value.to_owned());
        } else if let Some(value) = kv.strip_prefix("rev=") {
            rev = Some(value.to_owned());
        }
    }
    (path, reff, rev)
}

/// upstream rev, no tree fetch
pub fn current_rev(expanded: &str) -> Result<String> {
    match parse(expanded)? {
        Target::Github { owner, repo, reff } => {
            let ref_str = reff.as_deref().unwrap_or("HEAD");
            Ok(gh_commit(&owner, &repo, ref_str)?.0)
        },
        Target::Git { url, reff, rev } => {
            // a pinned rev never moves; report it without touching the network
            if let Some(pinned) = rev {
                return Ok(pinned);
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
        },
        Target::Tarball { url } => {
            let resp = agent()
                .head(&url)
                .header("User-Agent", "tack")
                .call()
                .or_else(|_| {
                    agent()
                        .get(&url)
                        .header("User-Agent", "tack")
                        .call()
                        .map_err(Box::new)
                })
                .with_context(|| format!("probe {url}"))?;
            Ok(immutable_url_of(&resp, &url))
        },
    }
}

/// Fetch a `fixed` pin: download URL bytes, sha256 them as raw bytes (not NAR),
/// return the locked node plus the sha256 (used for the drift-display "rev").
/// Auto-detects `unpack` from URL extension when not supplied.
pub fn fetch_fixed_pin(url: &str, unpack: Option<Unpack>) -> Result<(Value, String)> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        bail!("fixed pins require a plain http(s) URL, got: {url}");
    }
    let mut resp = agent()
        .get(url)
        .header("User-Agent", "tack")
        .call()
        .with_context(|| format!("GET {url}"))?;
    let immutable_url = immutable_url_of(&resp, url);
    let mut bytes = Vec::new();
    resp.body_mut()
        .as_reader()
        .read_to_end(&mut bytes)
        .with_context(|| format!("read body of {url}"))?;
    let sha256 = nar::hash_bytes(&bytes);
    // detect from the user-supplied URL first; the immutable URL may have lost
    // the extension via a redirect (e.g. github archives -> codeload)
    let unpack = unpack.unwrap_or_else(|| {
        if Unpack::detect(url) == Unpack::Tarball
            || Unpack::detect(&immutable_url) == Unpack::Tarball
        {
            Unpack::Tarball
        } else {
            Unpack::File
        }
    });
    let node = json!({
        "type": "fixed",
        "url": immutable_url,
        "sha256": sha256,
        "unpack": unpack.as_str(),
    });
    Ok((node, sha256))
}

/// download a locked tree into `dir` for inspection; no narhash, no metadata.
/// fixed pins are flat content, not trees — caller skips those.
pub fn fetch_locked_tree_into(node: &Value, dir: &Path) -> Result<PathBuf> {
    let ty = node
        .get("type")
        .and_then(Value::as_str)
        .context("lock node missing type")?;
    match ty {
        "github" => {
            let owner = node
                .get("owner")
                .and_then(Value::as_str)
                .context("github node missing owner")?;
            let repo = node
                .get("repo")
                .and_then(Value::as_str)
                .context("github node missing repo")?;
            let rev = node
                .get("rev")
                .and_then(Value::as_str)
                .context("github node missing rev")?;
            download_github_tarball(owner, repo, rev, dir)
        },
        "git" => {
            let url = node
                .get("url")
                .and_then(Value::as_str)
                .context("git node missing url")?;
            let reff = node.get("ref").and_then(Value::as_str);
            let rev = node.get("rev").and_then(Value::as_str);
            let submodules = node
                .get("submodules")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            git_checkout(url, reff, rev, submodules, dir)?;
            let _ = fs::remove_dir_all(dir.join(".git"));
            Ok(dir.to_owned())
        },
        "tarball" => {
            let url = node
                .get("url")
                .and_then(Value::as_str)
                .context("tarball node missing url")?;
            let mut resp = agent()
                .get(url)
                .header("User-Agent", "tack")
                .call()
                .with_context(|| format!("GET {url}"))?;
            let format = detect_tar_format(url).with_context(|| format!("tarball {url}"))?;
            unpack_tar_stream(resp.body_mut().as_reader(), format, dir)
        },
        other => bail!("cannot inspect tree for lock type '{other}'"),
    }
}

/// fetch a tree by parsed URL into `dir`; no narhash, no metadata.
/// used when traversing tack transitives that have no committed lock.
pub fn fetch_tree_into(expanded: &str, submodules: bool, dir: &Path) -> Result<PathBuf> {
    match parse(expanded)? {
        Target::Github { owner, repo, reff } => {
            let ref_str = reff.as_deref().unwrap_or("HEAD");
            let (rev, _) = gh_commit(&owner, &repo, ref_str)?;
            download_github_tarball(&owner, &repo, &rev, dir)
        },
        Target::Git { url, reff, rev } => {
            git_checkout(&url, reff.as_deref(), rev.as_deref(), submodules, dir)?;
            let _ = fs::remove_dir_all(dir.join(".git"));
            Ok(dir.to_owned())
        },
        Target::Tarball { url } => {
            let mut resp = agent()
                .get(&url)
                .header("User-Agent", "tack")
                .call()
                .with_context(|| format!("GET {url}"))?;
            let format = detect_tar_format(&url).with_context(|| format!("tarball {url}"))?;
            unpack_tar_stream(resp.body_mut().as_reader(), format, dir)
        },
    }
}

/// fetch the tree, return (locked node, rev)
pub fn fetch_pin(expanded: &str, submodules: bool) -> Result<(Value, String)> {
    match parse(expanded)? {
        Target::Github { owner, repo, reff } => {
            let ref_str = reff.as_deref().unwrap_or("HEAD");
            let (rev, last_modified) = gh_commit(&owner, &repo, ref_str)?;
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
        },
        Target::Git {
            url,
            reff,
            rev: rev_arg,
        } => {
            let dir = tempfile::tempdir()?;
            let (rev, last_modified, refname) = git_checkout(
                &url,
                reff.as_deref(),
                rev_arg.as_deref(),
                submodules,
                dir.path(),
            )?;
            let _ = fs::remove_dir_all(dir.path().join(".git")).ok();
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
        },
        Target::Tarball { url } => {
            let mut resp = agent()
                .get(&url)
                .header("User-Agent", "tack")
                .call()
                .with_context(|| format!("GET {url}"))?;
            let immutable_url = immutable_url_of(&resp, &url);
            let last_modified = resp
                .headers()
                .get("Last-Modified")
                .and_then(|header| header.to_str().ok())
                .and_then(|header| epoch_from_http_date(header).ok())
                .unwrap_or(0);
            let format = detect_tar_format(&immutable_url)
                .or_else(|_| detect_tar_format(&url))
                .with_context(|| format!("tarball {url}"))?;

            let dir = tempfile::tempdir()?;
            let root = unpack_tar_stream(resp.body_mut().as_reader(), format, dir.path())?;
            let nar_hash = nar::hash_path(&root)?;
            let node = json!({
                "type": "tarball",
                "url": immutable_url,
                "narHash": nar_hash,
                "lastModified": last_modified,
            });
            Ok((node, immutable_url))
        },
    }
}

/// Locked URL for a tarball response
fn immutable_url_of(resp: &http::Response<Body>, fallback: &str) -> String {
    resp.headers()
        .get("Link")
        .and_then(|header| header.to_str().ok())
        .and_then(parse_link_immutable)
        .unwrap_or_else(|| {
            let uri = resp.get_uri().to_string();
            if uri.is_empty() {
                fallback.to_owned()
            } else {
                uri
            }
        })
}

/// Extract the immutable URL from an HTTP Link header per RFC 8288.
fn parse_link_immutable(header: &str) -> Option<String> {
    for raw_part in header.split(',') {
        let part = raw_part.trim();
        let (url_part, params) = part.split_once(';')?;
        let url = url_part
            .trim()
            .strip_prefix('<')
            .and_then(|inner| inner.strip_suffix('>'))?;
        for param in params.split(';') {
            let (key, raw_value) = param.trim().split_once('=')?;
            if key.trim().eq_ignore_ascii_case("rel") {
                let rel = raw_value.trim().trim_matches('"');
                if rel == "immutable" || rel == "immutable_link" {
                    return Some(url.to_owned());
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
    let after_query = url.split('?').next().unwrap_or(url);
    let path = after_query.split('#').next().unwrap_or(after_query);
    if ends_with_ci(path, ".tar.xz") || ends_with_ci(path, ".txz") {
        Ok(TarFormat::Xz)
    } else if ends_with_ci(path, ".tar.gz") || ends_with_ci(path, ".tgz") {
        Ok(TarFormat::Gz)
    } else if ends_with_ci(path, ".tar") {
        Ok(TarFormat::Plain)
    } else {
        bail!(
            "cannot infer tarball format from url (need .tar, .tar.gz/.tgz, or .tar.xz/.txz): \
             {url}"
        )
    }
}

/// case-insensitive ASCII suffix check that is bytes-based to dodge utf-8
/// slicing
fn ends_with_ci(path: &str, ext: &str) -> bool {
    let pb = path.as_bytes();
    let eb = ext.as_bytes();
    pb.len() >= eb.len() && pb[pb.len() - eb.len()..].eq_ignore_ascii_case(eb)
}

/// Unpack a tarball stream into `into`, strip the single top-level directory
/// and return the stripped root.
fn unpack_tar_stream<R>(reader: R, format: TarFormat, into: &Path) -> Result<PathBuf>
where
    R: Read,
{
    let decompressed: Box<dyn Read> = match format {
        TarFormat::Gz => Box::new(GzDecoder::new(reader)),
        TarFormat::Xz => Box::new(XzDecoder::new(reader)),
        TarFormat::Plain => Box::new(reader),
    };
    let mut archive = tar::Archive::new(decompressed);
    archive.set_preserve_permissions(true);
    archive
        .unpack(into)
        .with_context(|| format!("unpack into {}", into.display()))?;
    let mut dirs = fs::read_dir(into)?
        .filter_map(|entry| entry.ok().map(|item| item.path()))
        .filter(|path| path.is_dir());
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
        .header("User-Agent", "tack")
        .header("Accept", "application/vnd.github+json");
    if let Ok(token) = env::var("GITHUB_TOKEN").or_else(|_| env::var("GH_TOKEN")) {
        req = req.header("Authorization", &format!("Bearer {token}"));
    }
    let body = req
        .call()
        .with_context(|| format!("github api {owner}/{repo}@{reff}"))?
        .body_mut()
        .read_to_string()?;
    let parsed = serde_json::from_str::<Value>(&body)?;
    let rev = parsed["sha"]
        .as_str()
        .ok_or_else(|| anyhow!("no sha in github response for {owner}/{repo}@{reff}"))?
        .to_owned();
    let date = parsed["commit"]["committer"]["date"]
        .as_str()
        .ok_or_else(|| anyhow!("no commit date for {owner}/{repo}@{reff}"))?;
    Ok((rev, epoch_from_iso(date)?))
}

fn download_github_tarball(owner: &str, repo: &str, rev: &str, into: &Path) -> Result<PathBuf> {
    let url = format!("https://codeload.github.com/{owner}/{repo}/tar.gz/{rev}");
    let mut resp = agent()
        .get(&url)
        .header("User-Agent", "tack")
        .call()
        .with_context(|| format!("download {url}"))?;
    unpack_tar_stream(resp.body_mut().as_reader(), TarFormat::Gz, into)
}

/// check out `rev` (if given) or the tip of `reff` (or remote default) into
/// `into`; return (rev, time, refname)
fn git_checkout(
    url: &str,
    reff: Option<&str>,
    requested_rev: Option<&str>,
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
    if requested_rev.is_none() {
        fo.depth(1);
    }
    remote
        .fetch(&[&refname], Some(&mut fo), None)
        .with_context(|| format!("fetch {refname} from {url}"))?;

    let commit = match requested_rev {
        Some(pinned) => {
            repo.revparse_single(pinned)
                .with_context(|| format!("rev '{pinned}' not reachable from {refname} on {url}"))?
                .peel_to_commit()
                .with_context(|| format!("'{pinned}' is not a commit"))?
        },
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

fn branch_str(raw: Result<git2::Buf, git2::Error>) -> Option<String> {
    let buf = raw.ok()?;
    buf.as_str().ok().map(str::to_owned)
}

fn full_ref(reff: Option<&str>, default: impl FnOnce() -> Option<String>) -> String {
    match reff {
        Some(target) if target.starts_with("refs/") => target.to_owned(),
        Some(target) => format!("refs/heads/{target}"),
        None => default().unwrap_or_else(|| "HEAD".to_owned()),
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
fn epoch_from_http_date(input: &str) -> Result<i64> {
    let bytes = input.as_bytes();
    if bytes.len() < 29 {
        bail!("bad http date: {input}");
    }
    let slice = |range: Range<usize>| -> Result<&str> {
        input
            .get(range)
            .with_context(|| format!("bad http date: {input}"))
    };
    let parse_num = |range: Range<usize>| -> Result<i64> {
        slice(range)?
            .parse()
            .with_context(|| format!("bad http date: {input}"))
    };
    let day = parse_num(5..7)?;
    let month = match slice(8..11)? {
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
        name => bail!("bad month in http date: {name}"),
    };
    let year = parse_num(12..16)?;
    let hh = parse_num(17..19)?;
    let mi = parse_num(20..22)?;
    let ss = parse_num(23..25)?;
    Ok(days_from_civil(year, month, day) * 86400 + hh * 3600 + mi * 60 + ss)
}

/// iso8601 to unix seconds
fn epoch_from_iso(input: &str) -> Result<i64> {
    let bytes = input.as_bytes();
    if bytes.len() < 20 {
        bail!("bad timestamp: {input}");
    }
    let parse_num = |range: Range<usize>| -> Result<i64> {
        input
            .get(range)
            .with_context(|| format!("bad timestamp: {input}"))?
            .parse()
            .with_context(|| format!("bad timestamp: {input}"))
    };
    let (year, month, day) = (parse_num(0..4)?, parse_num(5..7)?, parse_num(8..10)?);
    let (hh, mi, ss) = (parse_num(11..13)?, parse_num(14..16)?, parse_num(17..19)?);
    Ok(days_from_civil(year, month, day) * 86400 + hh * 3600 + mi * 60 + ss)
}

/// days since 1970-01-01 (howard hinnant)
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let yoe = adjusted_year - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
#[expect(clippy::panic, reason = "panic is the test-failure coping mechanism")]
mod tests {
    use super::*;

    #[test]
    fn git_rev_query() {
        match parse("git+https://example.com/o/r?ref=main&rev=abc123").unwrap() {
            Target::Git { url, reff, rev } => {
                assert_eq!(url, "https://example.com/o/r");
                assert_eq!(reff.as_deref(), Some("main"));
                assert_eq!(rev.as_deref(), Some("abc123"));
            },
            Target::Github { .. } | Target::Tarball { .. } => panic!("expected git target"),
        }
        match parse("git+ssh://git@example.com/o/r?rev=deadbeef").unwrap() {
            Target::Git { reff, rev, .. } => {
                assert_eq!(reff, None);
                assert_eq!(rev.as_deref(), Some("deadbeef"));
            },
            Target::Github { .. } | Target::Tarball { .. } => panic!("expected git target"),
        }
    }

    #[test]
    fn github_rev_is_committish() {
        match parse("github:o/r?rev=abc123").unwrap() {
            Target::Github { reff, .. } => assert_eq!(reff.as_deref(), Some("abc123")),
            Target::Git { .. } | Target::Tarball { .. } => panic!("expected github target"),
        }
    }

    #[test]
    fn https_url_is_tarball() {
        match parse("https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz").unwrap() {
            Target::Tarball { url } => {
                assert_eq!(
                    url,
                    "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz"
                );
            },
            Target::Github { .. } | Target::Git { .. } => panic!("expected tarball target"),
        }
        match parse("http://example.com/release.tar.gz").unwrap() {
            Target::Tarball { .. } => {},
            Target::Github { .. } | Target::Git { .. } => panic!("expected tarball target"),
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
        let immutable = "<https://releases.nixos.org/nixos/abc/nixexprs.tar.xz>; rel=\"immutable\"";
        assert_eq!(
            parse_link_immutable(immutable).as_deref(),
            Some("https://releases.nixos.org/nixos/abc/nixexprs.tar.xz")
        );

        // rel=immutable_link is the historic name used by some nix releases
        let immutable_link = "<https://x/y>; rel=\"immutable_link\"";
        assert_eq!(
            parse_link_immutable(immutable_link).as_deref(),
            Some("https://x/y")
        );

        // a Link header without an immutable rel yields None, not the wrong URL
        let canonical = "<https://x/y>; rel=\"canonical\"";
        assert!(parse_link_immutable(canonical).is_none());

        // multiple values: the immutable one wins regardless of position
        let mixed = "<https://x/canon>; rel=\"canonical\", <https://x/imm>; rel=\"immutable\"";
        assert_eq!(
            parse_link_immutable(mixed).as_deref(),
            Some("https://x/imm")
        );
    }

    #[test]
    fn http_date_roundtrip() {
        // 1994-11-06T08:49:37Z = 784111777
        assert_eq!(
            epoch_from_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap(),
            784_111_777
        );
        epoch_from_http_date("bogus").unwrap_err();
        epoch_from_http_date("Sun, 06 Foo 1994 08:49:37 GMT").unwrap_err();
    }

    // our tarball nar hash must equal nix's narHash for this rev
    // cargo test -- --ignored
    #[test]
    #[ignore = "hits codeload.github.com"]
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
            nar::hash_path(&root).unwrap(),
            "sha256-nt/xmuXaJB/vWlRJ4wpdlYQCIgCzFR6QJwlRyhfNn5o="
        );
    }
}
