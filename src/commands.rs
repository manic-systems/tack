// SPDX-License-Identifier: EUPL-1.2

use std::{
    cmp,
    collections::{
        BTreeMap,
        BTreeSet,
        HashSet,
    },
    env,
    fs,
    iter,
    mem,
    path::{
        Path,
        PathBuf,
    },
    sync::{
        Mutex,
        atomic::{
            AtomicUsize,
            Ordering,
        },
    },
};

use anyhow::{
    Result,
    bail,
};
use rayon::prelude::*;
use serde_json::Value;

use crate::{
    fetch,
    lock,
    pins,
    pins::{
        PinType,
        Unpack,
    },
    shorturl,
    ui::{
        Display,
        PinStatus,
    },
};

const STARTER_TOML: &str = include_str!("../assets/pins.toml");
const RESOLVER_NIX: &str = include_str!("../.tack/default.nix");
const MARKER: &str = "# tack-managed resolver.";

fn dir() -> PathBuf {
    if let Some(dir) = env::var_os("TACK_DIR") {
        return PathBuf::from(dir);
    }
    let cwd = env::current_dir().expect("cwd");
    if cwd.join("inputs.nix").exists() {
        return cwd;
    }
    cwd.join(".tack")
}

fn pins_path(dir: &Path) -> PathBuf {
    dir.join("pins.toml")
}
fn lock_path(dir: &Path) -> PathBuf {
    dir.join("pins.lock.json")
}

/// resolver is `default.nix` in the modern `.tack/` layout, or `inputs.nix`
/// when the dir is a repo root carrying the legacy layout
fn resolver_path(dir: &Path) -> PathBuf {
    let legacy = dir.join("inputs.nix");
    if legacy.exists() {
        return legacy;
    }
    dir.join("default.nix")
}

/// rewrite the resolver if it carries the management marker AND its bytes
/// differ from the bundled template; leave it alone otherwise.
fn refresh_resolver(dir: &Path) {
    let path = resolver_path(dir);
    if let Ok(current) = fs::read_to_string(&path)
        && current.contains(MARKER)
        && current != RESOLVER_NIX
    {
        let _ = write_atomic(&path, RESOLVER_NIX);
    }
}

fn short(rev: &str) -> String {
    fn trim(seg: &str) -> &str {
        let str = seg.split('?').next().unwrap_or(seg);
        str.split('#').next().unwrap_or(str)
    }
    if rev.contains("://") {
        let segs = rev
            .split_once("://")
            .map_or("", |x| x.1)
            .split('/')
            .filter(|seg| !seg.is_empty())
            .collect::<Vec<&str>>();

        let pick = match segs.len() {
            0 => None,
            1 => Some(trim(segs[0])),
            n => Some(trim(segs[n - 2])),
        };

        if let Some(seg) = pick {
            return seg.chars().take(16).collect();
        }
    }
    if let Some(b64) = rev.strip_prefix("sha256-") {
        return format!("sha256-{}", b64.chars().take(12).collect::<String>());
    }
    rev.chars().take(7).collect()
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let mut tmp_str = path.as_os_str().to_owned();
    tmp_str.push(".tmp");
    let tmp = PathBuf::from(tmp_str);
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn init(force: bool) -> Result<()> {
    let dir = dir();
    let (pt, lp, rp) = (pins_path(&dir), lock_path(&dir), resolver_path(&dir));

    if !force {
        let clash = [&pt, &rp]
            .into_iter()
            .filter_map(|path| path.exists().then_some(path.display().to_string()))
            .collect::<Vec<String>>();
        if !clash.is_empty() {
            bail!("{} already exists (use --force)", clash.join(", "));
        }
    }
    fs::create_dir_all(&dir)?;
    write_atomic(&pt, STARTER_TOML)?;
    if !lp.exists() {
        write_atomic(&lp, "{}\n")?;
    }
    write_atomic(&rp, RESOLVER_NIX)?;

    println!("initialised tack in {}", dir.display());
    println!("  pins.toml       edit shorturls and inputs here");
    println!("  pins.lock.json  written by `tack update`");
    println!("  default.nix     `import ./.tack` from your flake/config");
    Ok(())
}

pub fn add(
    name: &str,
    url: &str,
    pin_type: PinType,
    unpack: Option<Unpack>,
    dir_field: Option<&str>,
    submodules: bool,
    follows: &[(String, String)],
) -> Result<()> {
    if unpack.is_some() && pin_type != PinType::Fixed {
        bail!("--unpack is only valid with --fixed");
    }
    let dir = dir();
    let mut doc = pins::load(&pins_path(&dir))?;
    if pins::has_input(&doc, name) {
        bail!("input '{name}' already exists");
    }
    pins::add_input(&mut doc, name, url, &pins::AddInputOpts {
        pin_type,
        unpack,
        dir: dir_field,
        submodules,
        follows,
    });
    pins::save(&pins_path(&dir), &doc)?;

    let expanded = shorturl::expand(url, &pins::shorturls(&doc));
    let fetched = match pin_type {
        PinType::Fixed => fetch::fetch_fixed_pin(&expanded, unpack),
        PinType::Flake | PinType::Fetch => fetch::fetch_pin(&expanded, submodules),
    };
    match fetched {
        Ok((node, rev)) => {
            let mut lk = lock::load(&lock_path(&dir))?;
            lk.insert(name.to_owned(), node);
            lock::save(&lock_path(&dir), &lk)?;
            println!("added {name}  NEW -> {}", short(&rev));
        },
        Err(err) => {
            println!("added {name} to pins.toml, but locking failed: {err:#}");
            println!("  fix the url and run `tack update {name}`");
        },
    }
    refresh_resolver(&dir);
    Ok(())
}

pub fn rm(name: &str) -> Result<()> {
    let dir = dir();
    let mut doc = pins::load(&pins_path(&dir))?;
    if !pins::remove_input(&mut doc, name) {
        bail!("no input '{name}'");
    }
    pins::save(&pins_path(&dir), &doc)?;

    let mut lk = lock::load(&lock_path(&dir))?;
    lk.remove(name);
    lock::save(&lock_path(&dir), &lk)?;
    println!("removed {name}");
    refresh_resolver(&dir);
    Ok(())
}

pub fn alias(name: &str, template: Option<&str>, remove: bool) -> Result<()> {
    let dir = dir();
    let mut doc = pins::load(&pins_path(&dir))?;
    if remove {
        if !pins::remove_alias(&mut doc, name) {
            bail!("no alias '{name}'");
        }
        pins::save(&pins_path(&dir), &doc)?;
        println!("removed alias {name}");
    } else {
        let tpl = template.expect("template required");
        if !tpl.contains("{path}") {
            bail!("alias template must contain '{{path}}'");
        }
        pins::set_alias(&mut doc, name, tpl);
        pins::save(&pins_path(&dir), &doc)?;
        println!("alias {name} = {tpl}");
    }
    refresh_resolver(&dir);
    Ok(())
}

pub fn update(names: &[String], accept: bool) -> Result<()> {
    let dir = dir();
    let doc = pins::load(&pins_path(&dir))?;
    let shorturls = pins::shorturls(&doc);
    let all = pins::inputs(&doc)?;
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(());
    }
    let mut lk = lock::load(&lock_path(&dir))?;

    let display = Display::new(selected.iter().map(|i| i.name.clone()).collect());
    let drift = AtomicUsize::new(0);

    let results = selected
        .par_iter()
        .enumerate()
        .map(|(i, inp)| {
            display.set(i, PinStatus::Fetching);
            let expanded = shorturl::expand(&inp.url, &shorturls);
            let old = lk.get(&inp.name);
            let old_rev = old.and_then(lock::rev_of);
            let fetched = match inp.pin_type {
                PinType::Fixed => fetch::fetch_fixed_pin(&expanded, inp.unpack),
                PinType::Flake | PinType::Fetch => fetch::fetch_pin(&expanded, inp.submodules),
            };
            match fetched {
                // for fixed pins sha256 is the identity; any mismatch is drift
                Ok((node, rev))
                    if inp.pin_type == PinType::Fixed
                        && old_rev.is_some()
                        && old_rev != Some(rev.as_str()) =>
                {
                    display.set(i, PinStatus::FixedDrift {
                        old:      old_rev.map(short).unwrap_or_default(),
                        new:      short(&rev),
                        accepted: accept,
                    });
                    if accept {
                        Some(node)
                    } else {
                        drift.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                },
                Ok((node, rev)) if old_rev == Some(rev.as_str()) => {
                    // same rev, if hash moved, upstream changed under a stable rev
                    let drifted = matches!(
                        (old.and_then(lock::hash_of), lock::hash_of(&node)),
                        (Some(prev), Some(curr)) if prev != curr
                    );
                    if drifted {
                        display.set(i, PinStatus::Drift {
                            rev:      short(&rev),
                            accepted: accept,
                        });
                        if accept {
                            Some(node)
                        } else {
                            drift.fetch_add(1, Ordering::Relaxed);
                            None
                        }
                    } else {
                        display.set(i, PinStatus::NoChange);
                        None
                    }
                },
                Ok((node, rev)) => {
                    display.set(i, PinStatus::Updated {
                        old: old_rev.map_or_else(|| "NEW".into(), short),
                        new: short(&rev),
                    });
                    Some(node)
                },
                Err(err) => {
                    display.set(i, PinStatus::Failed(format!("{err:#}")));
                    None
                },
            }
        })
        .collect::<Vec<Option<Value>>>();

    let mut changed = false;
    for (inp, result) in selected.iter().zip(results) {
        if let Some(node) = result {
            lk.insert(inp.name.clone(), node);
            changed = true;
        }
    }
    let all_follow = pins::all_follows(&doc);
    if write_auto_dedup(&all, &all_follow, &mut lk) {
        changed = true;
    }
    if changed {
        lock::save(&lock_path(&dir), &lk)?;
    }
    display.finish();
    refresh_resolver(&dir);

    if drift.into_inner() > 0 {
        bail!(
            "upstream content differs from lock (lock kept; investigate, then re-run with \
             --accept to relock)"
        );
    }
    Ok(())
}

/// for every `[all_follow]` entry whose target isn't a declared `[inputs]` pin,
/// walk all top-level flake.locks once, collect every transitive observation
/// of the aliased name, and write the freshest by `lastModified` to
/// pins.lock.json under the target. also prunes stale auto-dedup entries that
/// no longer have a route.
fn write_auto_dedup(
    inputs: &[pins::Input],
    all_follow: &BTreeMap<String, String>,
    lock: &mut lock::Lock,
) -> bool {
    let input_names = inputs
        .iter()
        .map(|i| i.name.as_str())
        .collect::<HashSet<&str>>();

    let aliases = all_follow
        .iter()
        .filter(|&(_, target)| !input_names.contains(target.as_str()))
        .map(|(alias, target)| (alias.clone(), target.clone()))
        .collect::<BTreeMap<String, String>>();

    let mut valid = inputs
        .iter()
        .map(|i| i.name.clone())
        .collect::<HashSet<String>>();
    for target in aliases.values() {
        valid.insert(target.clone());
    }

    let stale = lock
        .keys()
        .filter(|key| !valid.contains(key.as_str()))
        .cloned()
        .collect::<Vec<String>>();
    let mut changed = false;
    for key in stale {
        lock.remove(&key);
        changed = true;
    }

    if aliases.is_empty() {
        return changed;
    }

    let batches = {
        let lock_ro: &lock::Lock = lock;
        inputs
            .par_iter()
            .filter(|inp| inp.pin_type == PinType::Flake)
            .filter_map(|inp| {
                let node = lock_ro.get(&inp.name)?;
                let raw = try_raw_file(node, "flake.lock")?;
                let parsed = serde_json::from_str::<Value>(&raw).ok()?;
                let root_key = parsed
                    .get("root")
                    .and_then(Value::as_str)
                    .unwrap_or("root")
                    .to_owned();
                let nodes = parsed.get("nodes")?.as_object()?.clone();
                let mut local = Vec::<(String, i64, Value)>::new();
                for (key, n) in &nodes {
                    if *key == root_key {
                        continue;
                    }
                    let stripped = strip_disambiguator(key);
                    let Some(target) = aliases.get(stripped) else {
                        continue;
                    };
                    let Some(locked) = n.get("locked") else {
                        continue;
                    };
                    let lm = locked
                        .get("lastModified")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    local.push((target.clone(), lm, locked.clone()));
                }
                Some(local)
            })
            .collect::<Vec<Vec<(String, i64, Value)>>>()
    };

    let mut observations = BTreeMap::<String, Vec<(i64, Value)>>::new();
    for batch in batches {
        for (target, lm, locked) in batch {
            observations.entry(target).or_default().push((lm, locked));
        }
    }

    for (target, mut obs) in observations {
        obs.sort_by_key(|entry| cmp::Reverse(entry.0));
        if let Some((_, winner)) = obs.into_iter().next()
            && lock.get(&target) != Some(&winner)
        {
            lock.insert(target, winner);
            changed = true;
        }
    }

    changed
}

pub fn look(names: &[String], verbose: bool) -> Result<()> {
    const LOG_LIMIT: usize = 5;

    let dir = dir();
    let doc = pins::load(&pins_path(&dir))?;
    let shorturls = pins::shorturls(&doc);
    let all = pins::inputs(&doc)?;
    if all.is_empty() {
        println!(
            "no pins in {}; add one with `tack add <name> <url>`",
            pins_path(&dir).display()
        );
        return Ok(());
    }
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(());
    }
    let lk = lock::load(&lock_path(&dir))?;

    let display = Display::new(selected.iter().map(|inp| inp.name.clone()).collect());
    let logs: Vec<Mutex<Option<fetch::CommitLog>>> = iter::repeat_with(|| Mutex::new(None))
        .take(selected.len())
        .collect();

    selected.par_iter().enumerate().for_each(|(i, inp)| {
        if inp.pin_type == PinType::Fixed {
            display.set(
                i,
                PinStatus::Skipped("fixed pin, run `tack update` to verify".into()),
            );
            return;
        }
        display.set(i, PinStatus::Fetching);
        let expanded = shorturl::expand(&inp.url, &shorturls);
        let old = lk.get(&inp.name).and_then(lock::rev_of).map(str::to_owned);
        match fetch::current_rev(&expanded) {
            Ok(rev) if old.as_deref() == Some(rev.as_str()) => {
                display.set(i, PinStatus::NoChange);
            },
            Ok(rev) => {
                display.set(i, PinStatus::Updated {
                    old: old.as_deref().map_or_else(|| "NEW".into(), short),
                    new: short(&rev),
                });
                if verbose
                    && let Some(old_rev) = old.as_deref()
                    && let Ok(Some(log)) =
                        fetch::commits_between(&expanded, old_rev, &rev, LOG_LIMIT)
                {
                    *logs[i].lock().unwrap() = Some(log);
                }
            },
            Err(err) => display.set(i, PinStatus::Failed(format!("{err:#}"))),
        }
    });

    if verbose {
        let collected = logs
            .into_iter()
            .map(|mutex| mutex.into_inner().unwrap())
            .collect::<Vec<_>>();
        display.finish_verbose(&collected);
    } else {
        display.finish();
    }
    Ok(())
}

/// named inputs, or all when empty
fn select<'a>(inputs: &'a [pins::Input], names: &[String]) -> Vec<&'a pins::Input> {
    if names.is_empty() {
        return inputs.iter().collect();
    }
    let mut out = Vec::new();
    for n in names {
        match inputs.iter().find(|i| &i.name == n) {
            Some(i) => out.push(i),
            None => eprintln!("no input '{n}'"),
        }
    }
    out
}

struct Entry {
    /// lineage from top-pin down to the parent tree being scanned
    path: Vec<String>,
    name: String,
    rev:  String,
}

struct Finding {
    identity: String,
    entry:    Entry,
}

struct ScanResult {
    findings:   Vec<Finding>,
    transitive: Vec<TackTransitive>,
}

struct TackTransitive {
    path:       Vec<String>,
    source:     SourceRef,
    submodules: bool,
}

enum SourceRef {
    Locked(Value),
    Url(String),
}

/// rev the consumer's resolver would actually use for this entry once
/// follows apply. global `[all_follow]` wins, per-pin `follows` fills in at
/// depth-1
fn effective_rev(
    path: &[String],
    name: &str,
    recorded: &str,
    top_input: Option<&pins::Input>,
    all_follow: &BTreeMap<String, String>,
    top_revs: &BTreeMap<String, String>,
) -> String {
    if path.is_empty() {
        return recorded.to_owned();
    }
    let excluded = top_input.is_some_and(|inp| inp.excludes.contains(name));
    if !excluded
        && let Some(target) = all_follow.get(name)
        && let Some(rev) = top_revs.get(target)
    {
        return rev.clone();
    }
    if path.len() == 1
        && let Some(inp) = top_input
        && let Some(target) = inp.follows.get(name)
        && let Some(rev) = top_revs.get(target)
    {
        return rev.clone();
    }
    recorded.to_owned()
}

pub fn dedup() -> Result<()> {
    let dir = dir();
    let doc = pins::load(&pins_path(&dir))?;
    let lock = lock::load(&lock_path(&dir))?;
    let inputs = pins::inputs(&doc)?;
    let shorturls = pins::shorturls(&doc);
    let all_follow = pins::all_follows(&doc);
    let by_name = inputs
        .iter()
        .map(|inp| (inp.name.as_str(), inp))
        .collect::<BTreeMap<&str, &pins::Input>>();

    let input_set = inputs
        .iter()
        .map(|inp| inp.name.as_str())
        .collect::<HashSet<&str>>();
    let top_revs = inputs
        .iter()
        .filter_map(|inp| {
            lock.get(&inp.name)
                .and_then(rev_for_display)
                .map(|rev| (inp.name.clone(), rev))
        })
        .chain(lock.iter().filter_map(|(key, node)| {
            (!input_set.contains(key.as_str()))
                .then(|| rev_for_display(node).map(|rev| (key.clone(), rev)))
                .flatten()
        }))
        .collect::<BTreeMap<String, String>>();

    let mut groups = BTreeMap::<String, Vec<Entry>>::new();

    for inp in &inputs {
        let expanded = shorturl::expand(&inp.url, &shorturls);
        if let Some(id) = canonical_identity(&expanded) {
            let rev = top_revs.get(&inp.name).cloned().unwrap_or_default();
            groups.entry(id).or_default().push(Entry {
                path: vec![],
                name: inp.name.clone(),
                rev,
            });
        }
    }

    let mut frontier = inputs
        .iter()
        .filter_map(|inp| {
            let node = lock.get(&inp.name)?;
            (inp.pin_type == PinType::Flake).then(|| {
                TackTransitive {
                    path:       vec![inp.name.clone()],
                    source:     SourceRef::Locked(node.clone()),
                    submodules: inp.submodules,
                }
            })
        })
        .collect::<Vec<TackTransitive>>();
    eprintln!("scanning {} pin(s)...", frontier.len());

    // bfs level-by-level: dedup the frontier against `visited`, fetch the
    // batch in parallel, then expand into the next frontier
    let mut visited = HashSet::<String>::new();
    while !frontier.is_empty() {
        let results = mem::take(&mut frontier)
            .into_iter()
            .filter(|item| visited.insert(source_key(&item.source)))
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|item| (item.path.clone(), fetch_and_scan(&item)))
            .collect::<Vec<_>>();

        for (path, res) in results {
            match res {
                Ok(scan) => {
                    for finding in scan.findings {
                        groups
                            .entry(finding.identity)
                            .or_default()
                            .push(finding.entry);
                    }
                    frontier.extend(scan.transitive);
                },
                Err(err) => eprintln!("tack: scan {}: {err:#}", path.join(" > ")),
            }
        }
    }

    for entries in groups.values_mut() {
        for entry in entries.iter_mut() {
            let top = entry
                .path
                .first()
                .and_then(|name| by_name.get(name.as_str()).copied());
            entry.rev = effective_rev(
                &entry.path,
                &entry.name,
                &entry.rev,
                top,
                &all_follow,
                &top_revs,
            );
        }
    }

    print_groups(&groups, &all_follow);
    Ok(())
}

fn fetch_and_scan(item: &TackTransitive) -> Result<ScanResult> {
    // fetch only the 3 files scan needs via http if the source's forge supports it
    if let SourceRef::Locked(ref node) = item.source
        && let Some((flake_lock, tack_pins, tack_lock)) = try_raw_files(node)
    {
        return Ok(scan_files(
            flake_lock.as_deref(),
            tack_pins.as_deref(),
            tack_lock.as_deref(),
            &item.path,
        ));
    }

    let tmp = tempfile::tempdir()?;
    let root = match item.source {
        SourceRef::Locked(ref node) => fetch::fetch_locked_tree_into(node, tmp.path())?,
        SourceRef::Url(ref url) => fetch::fetch_tree_into(url, item.submodules, tmp.path())?,
    };
    Ok(scan_tree(&root, &item.path))
}

/// `authoritative = true` means a 404 on every probe is final, and `false`
/// means the caller should fall back to clone
fn try_raw_files(node: &Value) -> Option<(Option<String>, Option<String>, Option<String>)> {
    let (_, authoritative) = forge_base(node)?;
    let triple = (
        try_raw_file(node, "flake.lock"),
        try_raw_file(node, ".tack/pins.toml"),
        try_raw_file(node, ".tack/pins.lock.json"),
    );
    if !authoritative && triple.0.is_none() && triple.1.is_none() && triple.2.is_none() {
        None
    } else {
        Some(triple)
    }
}

/// fetch one file from a locked node via raw http. returns [`None`] on unknown
/// host or http error
fn try_raw_file(node: &Value, file: &str) -> Option<String> {
    let (base, _) = forge_base(node)?;
    let rev = node.get("rev").and_then(Value::as_str)?;
    let (url, decoder) = forge_raw(&base, rev, file);
    let body = fetch::raw(&url).ok()?;
    if let Some(decode) = decoder {
        decode(&body).ok()
    } else {
        Some(body)
    }
}

/// (base url, authoritative) for raw-file probes. authoritative = true means
/// a 404 is definitive
fn forge_base(node: &Value) -> Option<(String, bool)> {
    match node.get("type").and_then(Value::as_str)? {
        "github" => {
            let owner = node.get("owner").and_then(Value::as_str)?;
            let repo = node.get("repo").and_then(Value::as_str)?;
            Some((
                format!("https://raw.githubusercontent.com/{owner}/{repo}"),
                true,
            ))
        },
        "gitlab" => {
            let owner = node.get("owner").and_then(Value::as_str)?;
            let repo = node.get("repo").and_then(Value::as_str)?;
            let host = node
                .get("host")
                .and_then(Value::as_str)
                .unwrap_or("gitlab.com");
            Some((format!("https://{host}/{owner}/{repo}"), true))
        },
        "git" => {
            let url = node.get("url").and_then(Value::as_str)?;
            Some((url.strip_suffix(".git").unwrap_or(url).to_owned(), false))
        },
        _ => None,
    }
}

/// body decoder applied after the http get
type Decoder = fn(&str) -> Result<String>;

/// map a repo base url + rev + file path to (raw-file url, optional decoder)
fn forge_raw(base: &str, rev: &str, file: &str) -> (String, Option<Decoder>) {
    let host = base
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("");
    if host == "raw.githubusercontent.com" {
        (format!("{base}/{rev}/{file}"), None)
    } else if host == "gitlab.com" || host.starts_with("gitlab.") {
        (format!("{base}/-/raw/{rev}/{file}"), None)
    } else if host == "bitbucket.org" {
        (format!("{base}/raw/{rev}/{file}"), None)
    } else if host.starts_with("cgit.") || host == "git.kernel.org" {
        (format!("{base}/plain/{file}?id={rev}"), None)
    } else if host.ends_with(".googlesource.com") || host.starts_with("gerrit.") {
        // gerrit/gitiles returns base64-encoded text under ?format=TEXT
        (
            format!("{base}/+/{rev}/{file}?format=TEXT"),
            Some(decode_b64),
        )
    } else {
        // forgejo / gitea / codeberg / unknown self-hosted
        (format!("{base}/raw/commit/{rev}/{file}"), None)
    }
}

fn decode_b64(body: &str) -> Result<String> {
    let bytes = data_encoding::BASE64
        .decode(body.trim().as_bytes())
        .map_err(|err| anyhow::anyhow!("base64 decode: {err}"))?;
    String::from_utf8(bytes).map_err(|err| anyhow::anyhow!("utf-8 decode: {err}"))
}

fn scan_tree(root: &Path, path: &[String]) -> ScanResult {
    let flake_lock = fs::read_to_string(root.join("flake.lock")).ok();
    let td = root.join(".tack");
    let tack_pins = fs::read_to_string(td.join("pins.toml")).ok();
    let tack_lock = fs::read_to_string(td.join("pins.lock.json")).ok();
    scan_files(
        flake_lock.as_deref(),
        tack_pins.as_deref(),
        tack_lock.as_deref(),
        path,
    )
}

fn scan_files(
    flake_lock: Option<&str>,
    tack_pins: Option<&str>,
    tack_lock: Option<&str>,
    path: &[String],
) -> ScanResult {
    let mut findings = Vec::<Finding>::new();
    let mut transitive = Vec::<TackTransitive>::new();

    if let Some(raw) = flake_lock
        && let Ok(json) = serde_json::from_str::<Value>(raw)
    {
        let root_key = json.get("root").and_then(Value::as_str).unwrap_or("root");
        if let Some(nodes) = json.get("nodes").and_then(Value::as_object) {
            for (key, node) in nodes {
                if key == root_key {
                    continue;
                }
                let Some(locked) = node.get("locked") else {
                    continue;
                };
                if let Some(id) = node_identity(locked) {
                    findings.push(Finding {
                        identity: id,
                        entry:    Entry {
                            path: path.to_vec(),
                            name: strip_disambiguator(key).to_owned(),
                            rev:  rev_for_display(locked).unwrap_or_default(),
                        },
                    });
                }
            }
        }
    }

    if let Some(raw) = tack_pins
        && let Ok(doc) = pins::parse_doc(raw)
        && let Ok(tinputs) = pins::inputs(&doc)
    {
        let tlock = tack_lock
            .and_then(|str| lock::parse(str).ok())
            .unwrap_or_default();
        let tshort = pins::shorturls(&doc);
        for tinp in &tinputs {
            let expanded = shorturl::expand(&tinp.url, &tshort);
            if let Some(id) = canonical_identity(&expanded) {
                findings.push(Finding {
                    identity: id,
                    entry:    Entry {
                        path: path.to_vec(),
                        name: tinp.name.clone(),
                        rev:  tlock
                            .get(&tinp.name)
                            .and_then(rev_for_display)
                            .unwrap_or_default(),
                    },
                });
            }
            if tinp.pin_type != PinType::Fixed {
                let mut next = path.to_vec();
                next.push(tinp.name.clone());
                let source = tlock
                    .get(&tinp.name)
                    .map_or(SourceRef::Url(expanded), |node| {
                        SourceRef::Locked(node.clone())
                    });
                transitive.push(TackTransitive {
                    path: next,
                    source,
                    submodules: tinp.submodules,
                });
            }
        }
    }

    ScanResult {
        findings,
        transitive,
    }
}

fn canonical_identity(expanded: &str) -> Option<String> {
    let no_query = expanded.split('?').next().unwrap_or(expanded);
    let path = no_query.split('#').next().unwrap_or(no_query);
    let id = if let Some(body) = path.strip_prefix("github:") {
        let mut segs = body.split('/');
        let owner = segs.next()?;
        let repo = segs.next()?;
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        format!("github:{owner}/{repo}")
    } else if let Some(rest) = path.strip_prefix("git+") {
        format!("git+{rest}")
    } else if path.starts_with("http://") || path.starts_with("https://") {
        format!("tarball:{path}")
    } else {
        return None;
    };
    Some(id.to_lowercase())
}

fn node_identity(locked: &Value) -> Option<String> {
    let ty = locked.get("type")?.as_str()?;
    let id = match ty {
        "github" => {
            let owner = locked.get("owner")?.as_str()?;
            let repo = locked.get("repo")?.as_str()?;
            format!("github:{owner}/{repo}")
        },
        "git" => {
            let url = locked.get("url")?.as_str()?;
            let cut = url.split('?').next().unwrap_or(url);
            format!("git+{cut}")
        },
        "tarball" => format!("tarball:{}", locked.get("url")?.as_str()?),
        "indirect" => format!("indirect:{}", locked.get("id")?.as_str()?),
        "path" => format!("path:{}", locked.get("path")?.as_str()?),
        _ => return None,
    };
    Some(id.to_lowercase())
}

fn source_key(source: &SourceRef) -> String {
    match *source {
        SourceRef::Locked(ref node) => node_identity(node).unwrap_or_else(|| node.to_string()),
        SourceRef::Url(ref url) => url.clone(),
    }
}

fn rev_for_display(node: &Value) -> Option<String> {
    if let Some(rev) = node.get("rev").and_then(Value::as_str) {
        return Some(short(rev));
    }
    if let Some(url) = node.get("url").and_then(Value::as_str) {
        return Some(short(url));
    }
    if let Some(sha) = node.get("sha256").and_then(Value::as_str) {
        return Some(short(sha));
    }
    None
}

/// flake.lock disambiguates same-named nodes as `name_2`, `name_3`, ...;
/// recover the original input name so dedup groups by what the parent flake
/// actually declares
fn strip_disambiguator(key: &str) -> &str {
    let bytes = key.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i > 0 && i < bytes.len() && bytes[i - 1] == b'_' {
        key.get(..i - 1).unwrap_or(key)
    } else {
        key
    }
}

fn source_label(path: &[String]) -> String {
    if path.is_empty() {
        "top".into()
    } else {
        path.join(" > ")
    }
}

fn print_groups(groups: &BTreeMap<String, Vec<Entry>>, all_follow: &BTreeMap<String, String>) {
    const MAX_SOURCES: usize = 5;

    // alias -> target, paste-ready under [all_follow]
    let mut pin_follow = BTreeMap::<String, String>::new();
    let mut auto_follow = BTreeMap::<String, String>::new();
    let mut printed = 0_usize;

    for (id, entries) in groups {
        if entries.len() < 2 {
            continue;
        }
        // already aligned by follows: skip
        let mut revs = entries.iter().map(|entry| entry.rev.as_str());
        if let Some(first) = revs.next()
            && revs.all(|rev| rev == first)
        {
            continue;
        }

        printed += 1;
        println!("\n{id}  x{}", entries.len());

        // group by rev, then by name within rev
        let mut by_rev = BTreeMap::<&str, BTreeMap<&str, Vec<String>>>::new();
        for entry in entries {
            by_rev
                .entry(entry.rev.as_str())
                .or_default()
                .entry(entry.name.as_str())
                .or_default()
                .push(source_label(&entry.path));
        }

        let rw = by_rev.keys().map(|rev| rev.len()).max().unwrap_or(0);
        let nw = by_rev
            .values()
            .flat_map(|names| names.keys().map(|name| name.len()))
            .max()
            .unwrap_or(0);

        for (rev, names) in &by_rev {
            for (name, sources) in names {
                let shown = sources.len().min(MAX_SOURCES);
                for (idx, source) in sources.iter().take(shown).enumerate() {
                    let rev_cell = if idx == 0 { *rev } else { "" };
                    let name_cell = if idx == 0 { *name } else { "" };
                    println!("  {rev_cell:rw$}  {name_cell:nw$}  {source}");
                }
                if sources.len() > shown {
                    let extra = sources.len() - shown;
                    println!("  {empty:rw$}  {empty:nw$}  ...{extra} more", empty = "");
                }
            }
        }

        let top_name = entries
            .iter()
            .filter(|entry| entry.path.is_empty())
            .map(|entry| entry.name.as_str())
            .min();
        if let Some(top) = top_name {
            for entry in entries {
                if !entry.path.is_empty() && !all_follow.contains_key(&entry.name) {
                    pin_follow.insert(entry.name.clone(), top.to_owned());
                }
            }
        } else {
            let aliases = entries
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<BTreeSet<String>>();
            let canonical = pick_name(id, &aliases);
            for alias in &aliases {
                if !all_follow.contains_key(alias) {
                    auto_follow.insert(alias.clone(), canonical.clone());
                }
            }
        }
    }

    if printed == 0 {
        println!("no duplicate inputs found");
        return;
    }
    if pin_follow.is_empty() && auto_follow.is_empty() {
        return;
    }
    let pin_lines = collapse_follow(&pin_follow);
    let auto_lines = collapse_follow(&auto_follow);
    let kw = pin_lines
        .iter()
        .chain(auto_lines.iter())
        .map(|&(ref key, _)| key.len())
        .max()
        .unwrap_or(0);
    println!("\nshare via [all_follow] in pins.toml:");
    for &(ref key, ref rhs) in &pin_lines {
        println!("  {key:kw$} = {rhs}");
    }
    if !auto_lines.is_empty() {
        if !pin_lines.is_empty() {
            println!();
        }
        println!("  # auto-dedup (no top-level pin needed):");
        for &(ref key, ref rhs) in &auto_lines {
            println!("  {key:kw$} = {rhs}");
        }
    }
}

/// invert alias -> target into target -> aliases and emit one line per target.
/// single-alias groups use string form (`alias = "target"`) whereas multi-alias
/// groups collapse to array form (`target = ["a", "b"]`)
fn collapse_follow(follow: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut by_target = BTreeMap::<&str, BTreeSet<&str>>::new();
    for (alias, target) in follow {
        by_target
            .entry(target.as_str())
            .or_default()
            .insert(alias.as_str());
    }
    let mut lines = Vec::<(String, String)>::new();
    for (target, aliases) in &by_target {
        if aliases.len() == 1 {
            let alias = aliases.iter().next().copied().unwrap_or("");
            lines.push((alias.to_owned(), format!("\"{target}\"")));
        } else {
            let body = aliases
                .iter()
                .filter(|alias| **alias != *target)
                .map(|alias| format!("\"{alias}\""))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(((*target).to_owned(), format!("[{body}]")));
        }
    }
    lines
}

/// suggested top-level name for a transitive-only group. this uses the github
/// repo basename when available, else the shortest alias seen
fn pick_name(id: &str, aliases: &BTreeSet<String>) -> String {
    if let Some(rest) = id.strip_prefix("github:")
        && let Some((_, repo)) = rest.split_once('/')
    {
        return repo.trim_end_matches(".nix").replace('.', "-");
    }
    aliases
        .iter()
        .min_by_key(|name| (name.len(), name.as_str()))
        .cloned()
        .unwrap_or_default()
}

pub fn help() {
    println!(
        "tack: flake-like toml nix pins, lazily fetched and transformed

usage:
  tack [-h|--help|help]
  tack init [--force]
  tack update [names...] [--accept]
  tack look [names...] [--verbose|-v]
  tack add <name> <url> [--fetch|--fixed [--unpack tarball|file]]
                        [--dir <d>] [--submodules] [--follows c=p]...
  tack rm <name>
  tack alias <name> <template> | tack alias --rm <name>
  tack dedup

pin types: flake (default), fetch (source tree only), fixed (FOD)

tack lives in ./.tack/ by default
use `import ./.tack` to use inputs

"
    );
}

#[cfg(test)]
mod tests {
    use std::collections::{
        BTreeMap,
        BTreeSet,
    };

    use super::{
        collapse_follow,
        pick_name,
    };

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|&(alias, target)| (alias.to_owned(), target.to_owned()))
            .collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn collapse_single_alias_uses_string_form() {
        let lines = collapse_follow(&map(&[("nixpkgs", "nixpkgs")]));
        assert_eq!(lines, vec![("nixpkgs".into(), "\"nixpkgs\"".into())]);
    }

    #[test]
    fn collapse_multi_alias_uses_array_form_excluding_key() {
        let lines = collapse_follow(&map(&[
            ("git-hooks", "git-hooks"),
            ("git-hooks-nix", "git-hooks"),
        ]));
        assert_eq!(lines, vec![(
            "git-hooks".into(),
            "[\"git-hooks-nix\"]".into()
        )]);
    }

    #[test]
    fn collapse_multi_alias_when_target_is_not_an_alias() {
        let lines = collapse_follow(&map(&[("xwl-stable", "xwl"), ("xwl-unstable", "xwl")]));
        assert_eq!(lines, vec![(
            "xwl".into(),
            "[\"xwl-stable\", \"xwl-unstable\"]".into()
        )]);
    }

    #[test]
    fn pick_name_strips_dot_nix_and_flattens_dots() {
        assert_eq!(
            pick_name("github:cachix/git-hooks.nix", &set(&["git-hooks"])),
            "git-hooks"
        );
        assert_eq!(
            pick_name("github:nix-community/nixpkgs.lib", &set(&["nixpkgs-lib"])),
            "nixpkgs-lib"
        );
    }

    #[test]
    fn pick_name_falls_back_to_shortest_alias_for_non_github() {
        let aliases = set(&["my-pin", "the-tarball"]);
        assert_eq!(pick_name("tarball:https://x/y", &aliases), "my-pin");
    }
}
