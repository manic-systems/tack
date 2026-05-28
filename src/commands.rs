// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
        HashSet,
    },
    fs,
    path::{
        Path,
        PathBuf,
    },
    sync::atomic::{
        AtomicUsize,
        Ordering,
    },
};

use anyhow::{
    Result,
    bail,
};
use rayon::prelude::*;
use serde_json::Value;
use toml_edit::Item;

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
    if let Some(d) = std::env::var_os("TACK_DIR") {
        return PathBuf::from(d);
    }
    let cwd = std::env::current_dir().expect("cwd");
    if cwd.join(".tack").is_dir() {
        return cwd.join(".tack");
    }
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

/// resolver lives next to pins.toml; legacy root-mode keeps the historical
/// `inputs.nix` name, otherwise the new convention is `default.nix`.
fn resolver_path(d: &Path) -> PathBuf {
    let legacy = d.join("inputs.nix");
    if legacy.exists() {
        return legacy;
    }
    d.join("default.nix")
}

/// rewrite the resolver if it carries the management marker AND its bytes
/// differ from the bundled template; leave it alone otherwise.
fn refresh_resolver(d: &Path) {
    let rp = resolver_path(d);
    match std::fs::read_to_string(&rp) {
        Ok(current) => {
            if current.contains(MARKER) && current != RESOLVER_NIX {
                let _ = write_atomic(&rp, RESOLVER_NIX);
            }
        },
        Err(_) => {
            // resolver missing — let init/etc. handle it; not our job here
        },
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
        let clash: Vec<String> = [&pt, &rp]
            .into_iter()
            .filter_map(|path| path.exists().then_some(path.display().to_string()))
            .collect::<Vec<String>>();
        if !clash.is_empty() {
            bail!("{} already exists (use --force)", clash.join(", "));
        }
    }
    std::fs::create_dir_all(&dir)?;
    write_atomic(&pt, STARTER_TOML)?;
    if !lp.exists() {
        write_atomic(&lp, "{}\n")?;
    }
    write_atomic(&rp, RESOLVER_NIX)?;

    let resolver_name = rp
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("default.nix");
    let import_hint = if dir.ends_with(".tack") {
        "import ./.tack".to_string()
    } else {
        format!("import ./{resolver_name}")
    };

    println!("initialised tack in {}", dir.display());
    println!("  pins.toml       edit shorturls and inputs here");
    println!("  pins.lock.json  written by `tack update`");
    println!("  {resolver_name:<14}  `{import_hint}` from your flake/config");
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
    pins::add_input(
        &mut doc, name, url, pin_type, unpack, dir_field, submodules, follows,
    );
    pins::save(&pins_path(&dir), &doc)?;

    let expanded = shorturl::expand(url, &pins::shorturls(&doc));
    let fetched = match pin_type {
        PinType::Fixed => fetch::fetch_fixed_pin(&expanded, unpack),
        _ => fetch::fetch_pin(&expanded, submodules),
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
                _ => fetch::fetch_pin(&expanded, inp.submodules),
            };
            match fetched {
                // for fixed pins sha256 is the identity; any mismatch is drift
                Ok((node, rev))
                    if inp.pin_type == PinType::Fixed
                        && old_rev.is_some()
                        && old_rev != Some(rev.as_str()) =>
                {
                    let old_short = old_rev.map(short).unwrap_or_default();
                    let new_short = short(&rev);
                    match accept {
                        true => {
                            display.set(i, PinStatus::FixedDrift {
                                old:      old_short,
                                new:      new_short,
                                accepted: true,
                            });
                            Some(node)
                        },
                        false => {
                            drift.fetch_add(1, Ordering::Relaxed);
                            display.set(i, PinStatus::FixedDrift {
                                old:      old_short,
                                new:      new_short,
                                accepted: false,
                            });
                            None
                        },
                    }
                },
                Ok((node, rev)) if old_rev == Some(rev.as_str()) => {
                    // same rev, if hash moved, upstream changed under a stable rev
                    let drifted = matches!(
                        (old.and_then(lock::hash_of), lock::hash_of(&node)),
                        (Some(prev), Some(curr)) if prev != curr
                    );
                    match (drifted, accept) {
                        // relock to the drifted tree, the user vouched for it
                        (true, true) => {
                            display.set(i, PinStatus::Drift {
                                rev:      short(&rev),
                                accepted: true,
                            });
                            Some(node)
                        },
                        // trip the alarm and keep the locked node
                        (true, false) => {
                            drift.fetch_add(1, Ordering::Relaxed);
                            display.set(i, PinStatus::Drift {
                                rev:      short(&rev),
                                accepted: false,
                            });
                            None
                        },
                        (false, _) => {
                            display.set(i, PinStatus::NoChange);
                            None
                        },
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

pub fn look(names: &[String]) -> Result<()> {
    let dir = dir();
    let doc = pins::load(&pins_path(&dir))?;
    let shorturls = pins::shorturls(&doc);
    let all = pins::inputs(&doc)?;
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(());
    }
    let lk = lock::load(&lock_path(&dir))?;

    let display = Display::new(selected.iter().map(|i| i.name.clone()).collect());

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
            },
            Err(err) => display.set(i, PinStatus::Failed(format!("{err:#}"))),
        }
    });
    display.finish();
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
    parent: String,
    name:   String,
    rev:    String,
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

pub fn dedup(deep: bool) -> Result<()> {
    let dir = dir();
    let doc = pins::load(&pins_path(&dir))?;
    let lock = lock::load(&lock_path(&dir))?;
    let inputs = pins::inputs(&doc)?;
    let shorturls = pins::shorturls(&doc);
    let configured_follows = existing_follows(&doc);

    let mut groups: BTreeMap<String, Vec<Entry>> = BTreeMap::new();

    for inp in &inputs {
        let expanded = shorturl::expand(&inp.url, &shorturls);
        if let Some(id) = canonical_identity(&expanded) {
            let rev = lock
                .get(&inp.name)
                .and_then(rev_for_display)
                .unwrap_or_default();
            groups.entry(id).or_default().push(Entry {
                parent: "top".into(),
                name: inp.name.clone(),
                rev,
            });
        }
    }

    let mut frontier: Vec<TackTransitive> = inputs
        .iter()
        .filter(|i| i.pin_type != PinType::Fixed)
        .filter_map(|inp| {
            let node = lock.get(&inp.name)?;
            Some(TackTransitive {
                path:       vec![inp.name.clone()],
                source:     SourceRef::Locked(node.clone()),
                submodules: inp.submodules,
            })
        })
        .collect();
    eprintln!("scanning {} pin(s)...", frontier.len());

    // bfs level-by-level: dedup the frontier against `visited`, fetch the
    // batch in parallel, then expand into the next frontier (deep only).
    let mut visited: HashSet<String> = HashSet::new();
    while !frontier.is_empty() {
        let mut batch: Vec<TackTransitive> = Vec::with_capacity(frontier.len());
        for item in frontier.drain(..) {
            if visited.insert(source_key(&item.source)) {
                batch.push(item);
            }
        }

        let results: Vec<(Vec<String>, Result<ScanResult>)> = batch
            .into_par_iter()
            .map(|item| {
                let res = fetch_and_scan(&item);
                (item.path, res)
            })
            .collect();

        for (path, res) in results {
            match res {
                Ok(scan) => {
                    for f in scan.findings {
                        groups.entry(f.identity).or_default().push(f.entry);
                    }
                    if deep {
                        frontier.extend(scan.transitive);
                    }
                },
                Err(err) => eprintln!("tack: scan {}: {err:#}", path.join(" > ")),
            }
        }

        if !deep {
            break;
        }
    }

    print_groups(&groups, &inputs, &configured_follows);
    Ok(())
}

fn fetch_and_scan(item: &TackTransitive) -> Result<ScanResult> {
    let tmp = tempfile::tempdir()?;
    let root = match &item.source {
        SourceRef::Locked(node) => fetch::fetch_locked_tree_into(node, tmp.path())?,
        SourceRef::Url(url) => fetch::fetch_tree_into(url, item.submodules, tmp.path())?,
    };
    scan_tree(&root, &item.path)
}

fn scan_tree(root: &Path, path: &[String]) -> Result<ScanResult> {
    let mut findings: Vec<Finding> = Vec::new();
    let mut transitive: Vec<TackTransitive> = Vec::new();
    let parent_label = format!("via {}", path.join(" > "));

    if let Ok(raw) = fs::read_to_string(root.join("flake.lock"))
        && let Ok(json) = serde_json::from_str::<Value>(&raw)
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
                            parent: parent_label.clone(),
                            name:   strip_disambiguator(key).to_owned(),
                            rev:    rev_for_display(locked).unwrap_or_default(),
                        },
                    });
                }
            }
        }
    }

    if let Some(td) = find_tack_dir(root)
        && let Ok(doc) = pins::load(&td.join("pins.toml"))
        && let Ok(tinputs) = pins::inputs(&doc)
    {
        let tlock = lock::load(&td.join("pins.lock.json")).unwrap_or_default();
        let tshort = pins::shorturls(&doc);
        for tinp in &tinputs {
            let expanded = shorturl::expand(&tinp.url, &tshort);
            if let Some(id) = canonical_identity(&expanded) {
                findings.push(Finding {
                    identity: id,
                    entry:    Entry {
                        parent: parent_label.clone(),
                        name:   tinp.name.clone(),
                        rev:    tlock
                            .get(&tinp.name)
                            .and_then(rev_for_display)
                            .unwrap_or_default(),
                    },
                });
            }
            if tinp.pin_type != PinType::Fixed {
                let mut next = path.to_vec();
                next.push(tinp.name.clone());
                let source = match tlock.get(&tinp.name) {
                    Some(node) => SourceRef::Locked(node.clone()),
                    None => SourceRef::Url(expanded),
                };
                transitive.push(TackTransitive {
                    path: next,
                    source,
                    submodules: tinp.submodules,
                });
            }
        }
    }

    Ok(ScanResult {
        findings,
        transitive,
    })
}

fn find_tack_dir(root: &Path) -> Option<PathBuf> {
    let new_layout = root.join(".tack");
    if new_layout.join("pins.toml").exists() {
        return Some(new_layout);
    }
    // legacy: pins.toml + inputs.nix at repo root
    if root.join("pins.toml").exists() && root.join("inputs.nix").exists() {
        return Some(root.to_owned());
    }
    None
}

fn canonical_identity(expanded: &str) -> Option<String> {
    let no_query = expanded.split('?').next().unwrap_or(expanded);
    let no_query = no_query.split('#').next().unwrap_or(no_query);
    if let Some(body) = no_query.strip_prefix("github:") {
        let mut segs = body.split('/');
        let owner = segs.next()?;
        let repo = segs.next()?;
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        return Some(format!("github:{owner}/{repo}"));
    }
    if let Some(rest) = no_query.strip_prefix("git+") {
        return Some(format!("git+{rest}"));
    }
    if no_query.starts_with("http://") || no_query.starts_with("https://") {
        return Some(format!("tarball:{no_query}"));
    }
    None
}

fn node_identity(locked: &Value) -> Option<String> {
    let ty = locked.get("type")?.as_str()?;
    match ty {
        "github" => {
            let owner = locked.get("owner")?.as_str()?;
            let repo = locked.get("repo")?.as_str()?;
            Some(format!("github:{owner}/{repo}"))
        },
        "git" => {
            let url = locked.get("url")?.as_str()?;
            let cut = url.split('?').next().unwrap_or(url);
            Some(format!("git+{cut}"))
        },
        "tarball" => Some(format!("tarball:{}", locked.get("url")?.as_str()?)),
        "indirect" => Some(format!("indirect:{}", locked.get("id")?.as_str()?)),
        "path" => Some(format!("path:{}", locked.get("path")?.as_str()?)),
        _ => None,
    }
}

fn source_key(source: &SourceRef) -> String {
    match source {
        SourceRef::Locked(node) => node_identity(node).unwrap_or_else(|| node.to_string()),
        SourceRef::Url(url) => url.clone(),
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
        &key[..i - 1]
    } else {
        key
    }
}

fn existing_follows(doc: &toml_edit::DocumentMut) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(tbl) = doc.get("all_follow").and_then(Item::as_table) {
        for (key, _) in tbl {
            out.insert(key.to_owned());
        }
    }
    out
}

fn print_groups(
    groups: &BTreeMap<String, Vec<Entry>>,
    inputs: &[pins::Input],
    configured: &BTreeSet<String>,
) {
    let top_names: HashSet<&str> = inputs.iter().map(|i| i.name.as_str()).collect();
    let mut suggest: BTreeSet<String> = BTreeSet::new();
    let mut printed = 0usize;

    for (id, entries) in groups {
        if entries.len() < 2 {
            continue;
        }
        printed += 1;
        println!("\n{id}  x{}", entries.len());
        let pw = entries.iter().map(|e| e.parent.len()).max().unwrap_or(0);
        let nw = entries.iter().map(|e| e.name.len()).max().unwrap_or(0);
        for e in entries {
            println!(
                "  {:pw$}  {:nw$}  {}",
                e.parent,
                e.name,
                e.rev,
                pw = pw,
                nw = nw
            );
        }
        let has_top = entries.iter().any(|e| e.parent == "top");
        if has_top {
            for e in entries {
                if e.parent != "top"
                    && top_names.contains(e.name.as_str())
                    && !configured.contains(&e.name)
                {
                    suggest.insert(e.name.clone());
                }
            }
        }
    }

    if printed == 0 {
        println!("no duplicate inputs found");
        return;
    }
    if !suggest.is_empty() {
        println!("\nshare via [all_follow] in pins.toml:");
        for name in &suggest {
            println!("  {name} = \"{name}\"");
        }
    }
}

pub fn help() {
    println!(
        "tack: flake-like toml nix pins, lazily fetched and transformed

usage:
  tack [-h|--help|help]
  tack init [--force]
  tack update [names...] [--accept]
  tack look [names...]
  tack add <name> <url> [--fetch|--fixed [--unpack tarball|file]]
                        [--dir <d>] [--submodules] [--follows c=p]...
  tack rm <name>
  tack alias <name> <template> | tack alias --rm <name>
  tack dedup [--deep]

pin types: flake (default), fetch (source tree only), fixed (FOD)

tack lives in ./.tack/ by default
use `import ./.tack` to use inputs

"
    );
}
