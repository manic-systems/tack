// SPDX-License-Identifier: EUPL-1.2

use std::{
    cmp,
    collections::{
        BTreeMap,
        BTreeSet,
        HashMap,
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
    process::Command,
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
use toml_edit::Item;

use crate::{
    fetch,
    fetch::{
        BranchComparison,
        CompareStatus,
    },
    history,
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
const SCAFFOLD_FLAKE: &str = include_str!("../templates/default/flake.nix");
const MARKER: &str = "# tack-managed resolver.";

struct UpdateFetch {
    node:       Value,
    rev:        String,
    comparison: BranchComparison,
}

pub fn dir() -> PathBuf {
    if let Some(dir) = env::var_os("TACK_DIR") {
        return PathBuf::from(dir);
    }
    let cwd = env::current_dir().expect("cwd");
    if cwd.join("inputs.nix").exists() {
        return cwd;
    }
    cwd.join(".tack")
}

pub fn pins_path(dir: &Path) -> PathBuf {
    dir.join("pins.toml")
}
pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join("pins.lock.json")
}

/// resolver is `default.nix` in the modern `.tack/` layout, or `inputs.nix`
/// when the dir is a repo root carrying the legacy layout
pub fn resolver_path(dir: &Path) -> PathBuf {
    let legacy = dir.join("inputs.nix");
    if legacy.exists() {
        return legacy;
    }
    dir.join("default.nix")
}

/// warn when the resolver still carries tack's marker but has drifted from the
/// bundled template. this is silent for forked resolvers who've stripped the
/// marker and when uninitialised, so it never nags people who own their copy.
pub fn warn_stale_resolver() {
    let path = resolver_path(&dir());
    if let Ok(current) = fs::read_to_string(&path)
        && current.contains(MARKER)
        && current != RESOLVER_NIX
    {
        eprintln!(
            "tack: resolver at {} is out of date. run `tack init --resolver` to update",
            path.display()
        );
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

pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let mut tmp_str = path.as_os_str().to_owned();
    tmp_str.push(".tmp");
    let tmp = PathBuf::from(tmp_str);
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[expect(clippy::fn_params_excessive_bools, reason = "independent bool options")]
pub fn init(force: bool, resolver_only: bool, flake: bool, import_flake: bool) -> Result<()> {
    let dir = dir();
    let (pt, lp, rp) = (pins_path(&dir), lock_path(&dir), resolver_path(&dir));

    // `--resolver` only bumps the resolver to the bundled template
    if resolver_only {
        return write_resolver(&dir, &rp, force);
    }

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

    if import_flake {
        import_flake_inputs(&pt)?;
    }

    println!("initialised tack in {}", dir.display());
    println!("  pins.toml       edit shorturls and inputs here");
    println!("  pins.lock.json  written by `tack update`");
    println!("  default.nix     `import ./.tack` from your flake/config");

    flake_awareness(flake, &dir)?;
    Ok(())
}

/// (re)write just the resolver to the bundled template. refuses to clobber a
/// forked resolver (marker stripped) unless `force`.
fn write_resolver(dir: &Path, path: &Path, force: bool) -> Result<()> {
    if let Ok(current) = fs::read_to_string(path) {
        if current == RESOLVER_NIX {
            println!("resolver already up to date at {}", path.display());
            return Ok(());
        }
        if !current.contains(MARKER) && !force {
            bail!(
                "{} has no tack marker, refusing to overwrite (use --force)",
                path.display()
            );
        }
    }
    fs::create_dir_all(dir)?;
    write_atomic(path, RESOLVER_NIX)?;
    println!("updated resolver at {}", path.display());
    Ok(())
}

/// `--flake` scaffolds a wired flake and marks the project recomposable, but
/// only when no flake.nix exists. an existing flake.nix is the user's, never
/// tack's.
fn flake_awareness(scaffold: bool, dir: &Path) -> Result<()> {
    let cwd = env::current_dir()?;
    let path = cwd.join("flake.nix");

    if !path.exists() {
        if scaffold {
            write_atomic(&path, SCAFFOLD_FLAKE)?;
            mark_recomposable(dir)?;
            if dir != cwd.join(".tack") {
                eprintln!(
                    "tack: scaffolded flake.nix imports ./.tack but the resolver is at {} (adjust \
                     the import)",
                    dir.display()
                );
            }
            println!("  flake.nix       wired resolver entry; edit its outputs");
            println!("  pins.toml       marked recomposable for downstream follows");
        } else {
            println!("  hint: `tack init --flake` scaffolds a recomposable flake.nix");
        }
        return Ok(());
    }

    // never overwrite the user's flake, just reflect its wiring into pins.toml
    if scaffold {
        eprintln!("tack: flake.nix exists; left untouched (tack won't overwrite your flake)");
    }
    if fs::read_to_string(&path).is_ok_and(|text| wires_overrides(&text)) {
        mark_recomposable(dir)?;
        println!("  pins.toml       marked recomposable (flake.nix already wired)");
    } else {
        print_wiring_blurb();
    }
    Ok(())
}

/// whether `flake.nix` mentions `tackOverrides` in code rather than only a `#`
/// comment.
fn wires_overrides(flake: &str) -> bool {
    flake.lines().any(|line| {
        line.split_once('#')
            .map_or(line, |(code, _)| code)
            .contains("tackOverrides")
    })
}

/// set `[tack] recomposable = true`, preserving any existing `[tack]` keys.
fn mark_recomposable(dir: &Path) -> Result<()> {
    let path = pins_path(dir);
    let mut doc = pins::load(&path)?;
    if let Some(table) = doc.get_mut("tack").and_then(Item::as_table_mut) {
        table["recomposable"] = toml_edit::value(true);
    } else {
        let mut table = toml_edit::Table::new();
        table["recomposable"] = toml_edit::value(true);
        doc.insert("tack", Item::Table(table));
    }
    pins::save(&path, &doc)
}

fn print_wiring_blurb() {
    println!(
        "
flake.nix is not marked recomposable. to let downstream tack projects
override your pins, thread tackOverrides through outputs:

  outputs =
    {{ self, ... }}@args:
    let inputs = (import ./.tack) {{ overrides = args.tackOverrides or {{ }}; }};
    in {{ }};

then set `[tack] recomposable = true` in .tack/pins.toml."
    );
}

fn import_flake_inputs(pins_toml: &Path) -> Result<()> {
    let cwd = env::current_dir()?;
    let path = cwd.join("flake.nix");

    if path.exists() {
        // Load flake inputs as JSON
        let cmd = Command::new("nix-instantiate")
            .args([
                "--eval",
                "--strict",
                "--raw",
                "-E",
                "
                    let
                        flake = import ./flake.nix;
                    in
                    builtins.toJSON (flake.inputs or {})
                ",
            ])
            .output()?;
        let flake_inputs: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&cmd.stdout)?;

        let mut doc = pins::load(pins_toml)?;

        // Add each input to pins.toml
        for (key, value) in flake_inputs {
            let mut input = toml_edit::Table::new();
            if let Some(url) = value.get("url")
                && let Some(url_str) = url.as_str()
            {
                input["url"] = url_str.into();

                // flake = false;
                //   -> type = "fetch"
                if let Some(input_is_flake) = value.get("flake")
                    && input_is_flake.as_bool().is_some_and(|is_flake| !is_flake)
                {
                    input["type"] = "fetch".into();
                }

                // inputs.<x>.follows = "<y>";
                //   -> follows = { <x> = "<y>" }
                if let Some(input_follows) = value.get("inputs")
                    && let Some(flake_follows) = input_follows.as_object()
                {
                    let follows: toml_edit::InlineTable = flake_follows
                        .iter()
                        .filter_map(|(input_key, nested_follows)| {
                            nested_follows
                                .as_object()
                                .and_then(|obj| obj.get("follows"))
                                .and_then(|follows_value| follows_value.as_str())
                                .map(|follows_str| (input_key, follows_str))
                        })
                        .collect();
                    if !follows.is_empty() {
                        input["follows"] = follows.into();
                    }
                }

                doc["inputs"][&key] = input.into();
            } else {
                eprintln!("tack: ignoring flake.nix input without url");
            }
        }

        pins::save(pins_toml, &doc)
    } else {
        eprintln!("tack: flake.nix doesn't exist; not importing inputs");
        Ok(())
    }
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
    Ok(())
}

pub fn rm(name: &str) -> Result<()> {
    let dir = dir();
    let (removed_pin, removed_lock) = rm_in_dir(&dir, name)?;
    if removed_pin {
        println!("removed {name}");
    } else if removed_lock {
        println!("removed stale lock entry {name}");
    }
    Ok(())
}

fn rm_in_dir(dir: &Path, name: &str) -> Result<(bool, bool)> {
    let mut doc = pins::load(&pins_path(dir))?;
    let removed_pin = pins::remove_input(&mut doc, name);

    let mut lk = lock::load(&lock_path(dir))?;
    let removed_lock = lk.remove(name).is_some();

    if !removed_pin && !removed_lock {
        bail!("no input '{name}'");
    }

    if removed_pin {
        pins::save(&pins_path(dir), &doc)?;
    }
    if removed_lock {
        lock::save(&lock_path(dir), &lk)?;
    }
    Ok((removed_pin, removed_lock))
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
            let fetched = fetch_for_update(inp, &expanded, old_rev);
            match fetched {
                // for fixed pins sha256 is the identity; any mismatch is drift
                Ok(UpdateFetch { node, rev, .. })
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
                Ok(UpdateFetch { node, rev, .. }) if old_rev == Some(rev.as_str()) => {
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
                Ok(UpdateFetch {
                    node,
                    rev,
                    comparison,
                }) => {
                    display.set(i, PinStatus::Updated {
                        old: old_rev.map_or_else(|| "NEW".into(), short),
                        new: short(&rev),
                        comparison,
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

    if drift.into_inner() > 0 {
        bail!(
            "upstream content differs from lock (lock kept; investigate, then re-run with \
             --accept to relock)"
        );
    }
    Ok(())
}

fn fetch_for_update(
    inp: &pins::Input,
    expanded: &str,
    old_rev: Option<&str>,
) -> Result<UpdateFetch> {
    match inp.pin_type {
        PinType::Fixed => {
            fetch::fetch_fixed_pin(expanded, inp.unpack).map(|(node, rev)| {
                UpdateFetch {
                    node,
                    rev,
                    comparison: BranchComparison::none(),
                }
            })
        },
        PinType::Flake | PinType::Fetch => {
            fetch::fetch_pin_compared(expanded, inp.submodules, old_rev).map(|fetched| {
                UpdateFetch {
                    node:       fetched.node,
                    rev:        fetched.rev,
                    comparison: fetched.comparison,
                }
            })
        },
    }
}

/// for every `[all_follow]` entry whose target isn't a declared `[inputs]` pin,
/// walk all top-level flake.locks once, collect every transitive observation
/// of the aliased name, and write the freshest by branch comparison when
/// possible, falling back to `lastModified`. also prunes stale auto-dedup
/// entries that no longer have a route.
fn write_auto_dedup(
    inputs: &[pins::Input],
    all_follow: &BTreeMap<String, String>,
    lock: &mut lock::Lock,
) -> bool {
    let input_names = inputs
        .iter()
        .map(|i| i.name.as_str())
        .collect::<HashSet<&str>>();

    // synthesis observes flake.lock nodes, so project aliases onto the flake
    // side: `flake:` is rekeyed bare and `tack:` is dropped
    let aliases = all_follow
        .iter()
        .filter(|&(_, target)| !input_names.contains(target.as_str()))
        .filter_map(|(alias, target)| Some((pins::flake_side(alias)?.to_owned(), target.clone())))
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

    let mut compare_cache = HashMap::new();
    for (target, mut obs) in observations {
        if let Some(current) = lock.get(&target) {
            let lm = last_modified(current)
                .and_then(|lm| i64::try_from(lm).ok())
                .unwrap_or(0);
            obs.insert(0, (lm, current.clone()));
        }
        if let Some(winner) = choose_lock_observation(obs, |base, head| {
            compare_locked_nodes(&mut compare_cache, base, head)
        }) && lock.get(&target) != Some(&winner)
        {
            lock.insert(target, winner);
            changed = true;
        }
    }

    changed
}

fn choose_lock_observation(
    observations: Vec<(i64, Value)>,
    mut compare: impl FnMut(&Value, &Value) -> Option<CompareStatus>,
) -> Option<Value> {
    let mut iter = observations.into_iter();
    let mut winner = iter.next()?;
    for candidate in iter {
        match compare(&winner.1, &candidate.1) {
            Some(CompareStatus::Ahead) => winner = candidate,
            Some(CompareStatus::Behind | CompareStatus::Identical) => {},
            Some(CompareStatus::Diverged) | None => {
                if candidate.0 > winner.0 {
                    winner = candidate;
                }
            },
        }
    }
    Some(winner.1)
}

fn compare_locked_nodes(
    cache: &mut HashMap<(String, String, String, String), Option<CompareStatus>>,
    base: &Value,
    head: &Value,
) -> Option<CompareStatus> {
    let (owner, repo, base_rev, head_rev) = github_compare_parts(base, head)?;
    if base_rev == head_rev {
        return Some(CompareStatus::Identical);
    }
    let key = (owner, repo, base_rev, head_rev);
    if let Some(cached) = cache.get(&key) {
        return *cached;
    }
    let status = fetch::compare_status(&key.0, &key.1, &key.2, &key.3)
        .ok()
        .flatten();
    cache.insert(key, status);
    status
}

fn github_compare_parts(base: &Value, head: &Value) -> Option<(String, String, String, String)> {
    if base.get("type").and_then(Value::as_str)? != "github"
        || head.get("type").and_then(Value::as_str)? != "github"
    {
        return None;
    }
    let base_owner = base.get("owner")?.as_str()?;
    let base_repo = base.get("repo")?.as_str()?;
    let head_owner = head.get("owner")?.as_str()?;
    let head_repo = head.get("repo")?.as_str()?;
    if !base_owner.eq_ignore_ascii_case(head_owner) || !base_repo.eq_ignore_ascii_case(head_repo) {
        return None;
    }
    Some((
        base_owner.to_owned(),
        base_repo.to_owned(),
        base.get("rev")?.as_str()?.to_owned(),
        head.get("rev")?.as_str()?.to_owned(),
    ))
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
        match fetch::current_rev_compared(&expanded, old.as_deref()) {
            Ok(current) if old.as_deref() == Some(current.rev.as_str()) => {
                display.set(i, PinStatus::NoChange);
            },
            Ok(current) => {
                display.set(i, PinStatus::Updated {
                    old:        old.as_deref().map_or_else(|| "NEW".into(), short),
                    new:        short(&current.rev),
                    comparison: current.comparison,
                });
                if verbose
                    && let Some(old_rev) = old.as_deref()
                    && let Ok(Some(log)) =
                        fetch::commits_between(&expanded, old_rev, &current.rev, LOG_LIMIT)
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

/// which side of an upstream a finding came from. a `flake:`/`tack:`-scoped
/// follow only matches its own side, though a bare follow matches both.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Flake,
    Tack,
}

impl Side {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Flake => "flake",
            Self::Tack => "tack",
        }
    }
}

struct Entry {
    /// lineage from top-pin down to the parent tree being scanned
    path:     Vec<String>,
    name:     String,
    /// flake input vs upstream tack pin, for side-scoped follow matching
    side:     Side,
    /// abbreviated rev
    rev:      String,
    /// untruncated rev
    full_rev: String,
    /// `lastModified` of the locked node
    lm:       Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CompareJob {
    id:    String,
    owner: String,
    repo:  String,
    base:  String,
    head:  String,
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

const MAX_COMPARE_JOBS: usize = 100;
const MAX_LIVE_COMPARE_JOBS: usize = 8;

/// build a name → value map over declared inputs plus undeclared lock entries,
/// pulling each value with `project`. transitive-only names are left in the
/// lock
fn top_map<T>(
    inputs: &[pins::Input],
    lock: &lock::Lock,
    project: impl Fn(&Value) -> Option<T>,
) -> BTreeMap<String, T> {
    let declared = inputs
        .iter()
        .map(|inp| inp.name.as_str())
        .collect::<HashSet<&str>>();
    inputs
        .iter()
        .filter_map(|inp| {
            lock.get(&inp.name)
                .and_then(&project)
                .map(|val| (inp.name.clone(), val))
        })
        .chain(lock.iter().filter_map(|(key, node)| {
            (!declared.contains(key.as_str()))
                .then(|| project(node).map(|val| (key.clone(), val)))
                .flatten()
        }))
        .collect()
}

/// the top-level input `name` follows, when it resolves to a known rev. returns
/// [`None`] to keep the entry's recorded values.
///
/// note that `[all_follow]` applies at any depth, though a parent flake's own
/// `follows` only one level down
fn follow_target(
    path: &[String],
    name: &str,
    side: Side,
    top_input: Option<&pins::Input>,
    all_follow: &BTreeMap<String, String>,
    top_revs: &BTreeMap<String, String>,
) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    // a bare follow reaches both sides, a `flake:`/`tack:` follow only its own
    let scoped = format!("{}:{name}", side.as_str());
    let excluded = top_input.is_some_and(|inp| inp.excludes.contains(name));
    if !excluded
        && let Some(target) = all_follow.get(name).or_else(|| all_follow.get(&scoped))
        && top_revs.contains_key(target)
    {
        return Some(target.clone());
    }
    if path.len() == 1
        && let Some(inp) = top_input
        && let Some(target) = inp.follows.get(name).or_else(|| inp.follows.get(&scoped))
        && top_revs.contains_key(target)
    {
        return Some(target.clone());
    }
    None
}

/// align each followed entry onto its target, which provides rev, full rev and
/// `lastModified`.
fn apply_follows(
    groups: &mut BTreeMap<String, Vec<Entry>>,
    by_name: &BTreeMap<&str, &pins::Input>,
    all_follow: &BTreeMap<String, String>,
    top_revs: &BTreeMap<String, String>,
    top_full_revs: &BTreeMap<String, String>,
    top_lms: &BTreeMap<String, u64>,
) {
    for entry in groups.values_mut().flatten() {
        let top = entry
            .path
            .first()
            .and_then(|name| by_name.get(name.as_str()).copied());
        let Some(target) = follow_target(
            &entry.path,
            &entry.name,
            entry.side,
            top,
            all_follow,
            top_revs,
        ) else {
            continue;
        };
        // `follow_target` only returns targets present in `top_revs`, and
        // `top_full_revs` shares its key set
        if let Some(rev) = top_revs.get(&target) {
            entry.rev.clone_from(rev);
        }
        if let Some(full_rev) = top_full_revs.get(&target) {
            entry.full_rev.clone_from(full_rev);
        }
        entry.lm = top_lms.get(&target).copied();
    }
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

    let top_revs = top_map(&inputs, &lock, rev_for_display);
    let top_full_revs = top_map(&inputs, &lock, rev_full);
    let top_lms = top_map(&inputs, &lock, last_modified);

    let mut groups = BTreeMap::<String, Vec<Entry>>::new();

    for inp in &inputs {
        let expanded = shorturl::expand(&inp.url, &shorturls);
        if let Some(id) = canonical_identity(&expanded) {
            let rev = top_revs.get(&inp.name).cloned().unwrap_or_default();
            let full_rev = top_full_revs.get(&inp.name).cloned().unwrap_or_default();
            let lm = lock.get(&inp.name).and_then(last_modified);
            groups.entry(id).or_default().push(Entry {
                path: vec![],
                name: inp.name.clone(),
                side: Side::Flake,
                rev,
                full_rev,
                lm,
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

    apply_follows(
        &mut groups,
        &by_name,
        &all_follow,
        &top_revs,
        &top_full_revs,
        &top_lms,
    );

    let compares = ahead_behind(&groups);
    print_groups(&groups, &all_follow, &compares);
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
                            path:     path.to_vec(),
                            name:     strip_disambiguator(key).to_owned(),
                            side:     Side::Flake,
                            rev:      rev_for_display(locked).unwrap_or_default(),
                            full_rev: rev_full(locked).unwrap_or_default(),
                            lm:       last_modified(locked),
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
                        path:     path.to_vec(),
                        name:     tinp.name.clone(),
                        side:     Side::Tack,
                        rev:      tlock
                            .get(&tinp.name)
                            .and_then(rev_for_display)
                            .unwrap_or_default(),
                        full_rev: tlock.get(&tinp.name).and_then(rev_full).unwrap_or_default(),
                        lm:       tlock.get(&tinp.name).and_then(last_modified),
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

fn last_modified(node: &Value) -> Option<u64> {
    node.get("lastModified").and_then(Value::as_u64)
}

fn rev_for_display(node: &Value) -> Option<String> {
    rev_full(node).as_deref().map(short)
}

/// the locked node's identifying string, untruncated: `rev`, else `url`, else
/// `sha256`. [`rev_for_display`] is just this passed through [`short`].
fn rev_full(node: &Value) -> Option<String> {
    for key in ["rev", "url", "sha256"] {
        if let Some(val) = node.get(key).and_then(Value::as_str) {
            return Some(val.to_owned());
        }
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

/// the entry a group is measured against. returns the version pinned at top,
/// else the newest transitive version by `lastModified`, else the lowest-named
/// entry for a deterministic fallback
fn comparator(entries: &[Entry]) -> Option<&Entry> {
    entries
        .iter()
        .filter(|entry| entry.path.is_empty())
        .min_by_key(|entry| entry.name.as_str())
        .or_else(|| {
            entries
                .iter()
                .filter(|entry| entry.lm.is_some())
                .max_by_key(|entry| entry.lm)
        })
        .or_else(|| entries.iter().min_by_key(|entry| entry.name.as_str()))
}

/// a group is worth printing only when its revs disagree
fn group_diverges(entries: &[Entry]) -> bool {
    let mut revs = entries.iter().map(entry_compare_rev);
    revs.next()
        .is_some_and(|first| revs.any(|rev| rev != first))
}

const fn entry_compare_rev(entry: &Entry) -> &str {
    if entry.full_rev.is_empty() {
        entry.rev.as_str()
    } else {
        entry.full_rev.as_str()
    }
}

/// build github compare work for every divergent rev against its comparator.
/// returned jobs carry full revs for both the network request and the result
/// map keys; rendering remains separately abbreviated.
fn compare_jobs(groups: &BTreeMap<String, Vec<Entry>>) -> (Vec<CompareJob>, usize) {
    let mut jobs = groups
        .iter()
        .filter(|group| group_diverges(group.1))
        .filter_map(|(id, entries)| {
            let base = comparator(entries)?;
            if base.full_rev.is_empty() {
                return None; // nothing concrete to compare against
            }
            let (owner, repo) = id.strip_prefix("github:")?.split_once('/')?;
            let mut seen = HashSet::new();
            let heads = entries
                .iter()
                .filter(|entry| {
                    entry.full_rev != base.full_rev
                        && !entry.full_rev.is_empty()
                        && seen.insert(entry.full_rev.as_str())
                })
                .map(|entry| {
                    CompareJob {
                        id:    id.clone(),
                        owner: owner.to_owned(),
                        repo:  repo.to_owned(),
                        base:  base.full_rev.clone(),
                        head:  entry.full_rev.clone(),
                    }
                })
                .collect::<Vec<_>>();
            Some(heads)
        })
        .flatten()
        .collect::<Vec<_>>();

    let capped = jobs.len().saturating_sub(MAX_COMPARE_JOBS);
    jobs.truncate(MAX_COMPARE_JOBS);
    (jobs, capped)
}

/// ask github for the direction of every divergent rev against its comparator,
/// in bounded parallel batches. keyed by `(group id, full rev)`. misses are
/// reported and fall back to commit-date ordering.
fn ahead_behind(groups: &BTreeMap<String, Vec<Entry>>) -> HashMap<(String, String), CompareStatus> {
    let (jobs, capped) = compare_jobs(groups);
    let attempted = jobs.len();
    let mut compares = HashMap::<(String, String), CompareStatus>::new();
    for chunk in jobs.chunks(MAX_LIVE_COMPARE_JOBS) {
        compares.extend(
            chunk
                .into_par_iter()
                .filter_map(|job| {
                    fetch::compare_status(&job.owner, &job.repo, &job.base, &job.head)
                        .ok()
                        .flatten()
                        .map(|status| ((job.id.clone(), job.head.clone()), status))
                })
                .collect::<Vec<_>>(),
        );
    }

    let dropped = capped + attempted - compares.len();
    if dropped > 0 {
        eprintln!(
            "tack: {dropped} branch comparison(s) unavailable or capped; falling back to \
             commit-date order"
        );
    }
    compares
}

/// per-rev status glyph (rendered text + visible width) for a printable group,
/// measured against its comparator: github ahead/behind when we have it,
/// commit-date ordering otherwise. also returns the widest glyph, for padding
fn group_marks<'a>(
    id: &str,
    entries: &'a [Entry],
    revs: impl Iterator<Item = &'a str>,
    compares: &HashMap<(String, String), CompareStatus>,
) -> (BTreeMap<&'a str, (String, usize)>, usize) {
    // a plain "~" marks a date-based guess
    const APPROX: &str = "~";

    let comp_entry = comparator(entries);
    let mut lm_of = BTreeMap::<&str, u64>::new();
    for entry in entries {
        let Some(lm) = entry.lm else {
            continue;
        };
        let slot = lm_of.entry(entry_compare_rev(entry)).or_insert(lm);
        *slot = (*slot).max(lm);
    }
    let paint = |code: i32, body: &str| format!("\x1b[{code}m{body}\x1b[0m");
    let dated = |code: i32, arrow: &str| {
        (
            format!("{}{}", paint(code, arrow), paint(36_i32, APPROX)),
            2,
        )
    };
    let render = |rev: &str| -> (String, usize) {
        let Some(comp) = comp_entry else {
            return (" ".to_owned(), 1);
        };
        if rev == entry_compare_rev(comp) {
            return (paint(36_i32, "="), 1); // = comparator
        }
        if let Some(status) = compares.get(&(id.to_owned(), rev.to_owned())) {
            // real direction from github's merge-base comparison
            match *status {
                CompareStatus::Ahead => return (paint(32_i32, "\u{2191}"), 1), // ↑ newer
                CompareStatus::Behind => return (paint(33_i32, "\u{2193}"), 1), // ↓ older
                // diverged: green ↑ beside yellow ↓
                CompareStatus::Diverged => {
                    let glyph =
                        format!("{}{}", paint(32_i32, "\u{2191}"), paint(33_i32, "\u{2193}"));
                    return (glyph, 2);
                },
                CompareStatus::Identical => return (paint(36_i32, "="), 1),
            }
        }
        // no git answer (non-github, or the call failed): commit-date order
        let Some(comparator_lm) = comp.lm else {
            return (" ".to_owned(), 1);
        };
        let Some(lm) = lm_of.get(rev).copied() else {
            return (" ".to_owned(), 1);
        };
        match lm.cmp(&comparator_lm) {
            cmp::Ordering::Equal => (paint(36_i32, APPROX), 1),
            cmp::Ordering::Greater => dated(32_i32, "\u{2191}"), // ↑ newer by timestamp
            cmp::Ordering::Less => dated(33_i32, "\u{2193}"),    // ↓ older by timestamp
        }
    };
    let marks = revs
        .map(|rev| (rev, render(rev)))
        .collect::<BTreeMap<&'a str, (String, usize)>>();
    let mw = marks.values().map(|&(_, vis)| vis).max().unwrap_or(1);
    (marks, mw)
}

type SourcesByRev<'a> = BTreeMap<&'a str, (&'a str, BTreeMap<&'a str, Vec<String>>)>;

fn group_sources_by_rev(entries: &[Entry]) -> SourcesByRev<'_> {
    let mut by_rev = BTreeMap::<&str, (&str, BTreeMap<&str, Vec<String>>)>::new();
    for entry in entries {
        let names = &mut by_rev
            .entry(entry_compare_rev(entry))
            .or_insert_with(|| (entry.rev.as_str(), BTreeMap::new()))
            .1;
        names
            .entry(entry.name.as_str())
            .or_default()
            .push(source_label(&entry.path));
    }
    by_rev
}

fn print_groups(
    groups: &BTreeMap<String, Vec<Entry>>,
    all_follow: &BTreeMap<String, String>,
    compares: &HashMap<(String, String), CompareStatus>,
) {
    const MAX_SOURCES: usize = 5;

    // alias -> target, paste-ready under [all_follow]
    let mut pin_follow = BTreeMap::<String, String>::new();
    let mut auto_follow = BTreeMap::<String, String>::new();
    let mut printed = 0_usize;

    for (id, entries) in groups {
        // single source, or already aligned by follows: nothing to show
        if !group_diverges(entries) {
            continue;
        }

        printed += 1;
        println!("\n{id}  x{}", entries.len());

        let by_rev = group_sources_by_rev(entries);

        let rw = by_rev
            .values()
            .map(|&(rev, _)| rev.len())
            .max()
            .unwrap_or(0);
        let nw = by_rev
            .values()
            .flat_map(|&(_, ref names)| names.keys().map(|name| name.len()))
            .max()
            .unwrap_or(0);

        let (marks, mw) = group_marks(id, entries, by_rev.keys().copied(), compares);

        for (rev, &(display_rev, ref names)) in &by_rev {
            let mark = &marks[rev];
            let mark_on = format!("{}{}", mark.0, " ".repeat(mw - mark.1));
            let blank = " ".repeat(mw);
            for (name, sources) in names {
                let shown = sources.len().min(MAX_SOURCES);
                for (idx, source) in sources.iter().take(shown).enumerate() {
                    let rev_cell = if idx == 0 { display_rev } else { "" };
                    let mark_cell = if idx == 0 { &mark_on } else { &blank };
                    let name_cell = if idx == 0 { *name } else { "" };
                    println!("  {rev_cell:rw$} {mark_cell} {name_cell:nw$}  {source}");
                }
                if sources.len() > shown {
                    let extra = sources.len() - shown;
                    println!(
                        "  {empty:rw$} {blank} {empty:nw$}  ...{extra} more",
                        empty = ""
                    );
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

pub fn undo(list: bool) -> Result<()> {
    let dir = dir();
    let store = history::store_dir(&dir);
    if list {
        match history::list(&store) {
            Some(view) => render(&view, 0, view.rows.len().saturating_sub(1)),
            None => println!("no history"),
        }
        return Ok(());
    }
    match history::undo(&dir, &store)? {
        Some(view) => render_window(&view),
        None => println!("nothing to undo"),
    }
    Ok(())
}

pub fn redo() -> Result<()> {
    let dir = dir();
    let store = history::store_dir(&dir);
    match history::redo(&dir, &store)? {
        Some(view) => render_window(&view),
        None => println!("nothing to redo"),
    }
    Ok(())
}

/// a radius-1 window around the new cursor: the redo target, the live state,
/// the undo target.
fn render_window(view: &history::View) {
    let lo = view.cursor.saturating_sub(1);
    let hi = (view.cursor + 1).min(view.rows.len().saturating_sub(1));
    render(view, lo, hi);
}

/// rows `lo..=hi` newest-first, relative times aligned, `>` marking the cursor
fn render(view: &history::View, lo: usize, hi: usize) {
    let now = history::now();
    let times = (lo..=hi)
        .map(|idx| history::rel_time(now, view.rows[idx].ts))
        .collect::<Vec<String>>();
    let width = times.iter().map(String::len).max().unwrap_or(0);
    for idx in (lo..=hi).rev() {
        let marker = if idx == view.cursor { '>' } else { ' ' };
        let when = &times[idx - lo];
        println!("{marker} {when:width$}  {}", view.rows[idx].label);
    }
}

pub fn help() {
    println!(
        "tack: flake-like toml nix pins, lazily fetched and transformed

usage:
  tack [-h|--help|help]
  tack init [--force] [--resolver] [--flake] [--import-flake]
  tack update [names...] [--accept]
  tack look [names...] [--verbose|-v]
  tack add <name> <url> [--fetch|--fixed [--unpack tarball|file]]
                        [--dir <d>] [--submodules] [--follows c=p]...
  tack rm <name>
  tack alias <name> <template> | tack alias --rm <name>
  tack dedup
  tack undo [--list]
  tack redo

pin types: flake (default), fetch (source tree only), fixed (FOD)
follows keys may be scoped flake:<name> or tack:<name> (no prefix implies both)

tack lives in ./.tack/ by default
use `import ./.tack` to use inputs

run `tack init --resolver` to update a drifted resolver

"
    );
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{
            BTreeMap,
            BTreeSet,
            HashMap,
        },
        fs,
        iter,
    };

    use serde_json::Value;

    use super::{
        Entry,
        MAX_COMPARE_JOBS,
        Side,
        apply_follows,
        choose_lock_observation,
        collapse_follow,
        comparator,
        compare_jobs,
        group_diverges,
        group_marks,
        pick_name,
        rm_in_dir,
        wires_overrides,
    };

    #[test]
    fn wires_overrides_ignores_comments() {
        assert!(wires_overrides(
            "outputs = { self, ... }@args: (import ./.tack) { overrides = args.tackOverrides or \
             {}; };"
        ));
        // a commented-out mention must not trip the recomposable flag
        assert!(!wires_overrides(
            "# threads args.tackOverrides through outputs\n{ }"
        ));
        assert!(!wires_overrides(
            "outputs = { self }: { }; # no tackOverrides here"
        ));
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|&(alias, target)| (alias.to_owned(), target.to_owned()))
            .collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn tack_entry(path: &[&str], name: &str, rev: &str, lm: Option<u64>) -> Entry {
        Entry {
            side: Side::Tack,
            ..entry(path, name, rev, lm)
        }
    }

    fn entry(path: &[&str], name: &str, rev: &str, lm: Option<u64>) -> Entry {
        entry_full(path, name, rev, rev, lm)
    }

    fn entry_full(path: &[&str], name: &str, rev: &str, full_rev: &str, lm: Option<u64>) -> Entry {
        Entry {
            path: path.iter().map(|item| (*item).to_owned()).collect(),
            name: name.to_owned(),
            side: Side::Flake,
            rev: rev.to_owned(),
            full_rev: full_rev.to_owned(),
            lm,
        }
    }

    fn github_node(rev: &str) -> Value {
        serde_json::json!({
            "type": "github",
            "owner": "o",
            "repo": "r",
            "rev": rev,
        })
    }

    fn node_rev(node: &Value) -> &str {
        node.get("rev").and_then(Value::as_str).unwrap()
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

    #[test]
    fn comparator_prefers_top_level_even_without_last_modified() {
        let entries = vec![
            entry(&["parent"], "aaa", "newer", Some(20)),
            entry(&[], "top", "top-rev", None),
        ];
        assert_eq!(
            comparator(&entries).map(|entry| (entry.rev.as_str(), entry.lm)),
            Some(("top-rev", None))
        );
    }

    #[test]
    fn comparator_uses_newest_known_transitive_then_deterministic_fallback() {
        let entries_with_known_time = vec![
            entry(&["parent"], "aaa", "unknown", None),
            entry(&["parent"], "bbb", "older", Some(10)),
            entry(&["parent"], "ccc", "newer", Some(20)),
        ];
        assert_eq!(
            comparator(&entries_with_known_time).map(|entry| (entry.rev.as_str(), entry.lm)),
            Some(("newer", Some(20)))
        );

        let entries_without_times = vec![
            entry(&["parent"], "bbb", "unknown-b", None),
            entry(&["parent"], "aaa", "unknown-a", None),
        ];
        assert_eq!(
            comparator(&entries_without_times).map(|entry| (entry.rev.as_str(), entry.lm)),
            Some(("unknown-a", None))
        );
    }

    #[test]
    fn auto_dedup_prefers_ahead_candidate_despite_older_timestamp() {
        let winner = choose_lock_observation(
            vec![(300, github_node("base")), (100, github_node("ahead"))],
            |base, head| {
                match (node_rev(base), node_rev(head)) {
                    ("base", "ahead") => Some(super::CompareStatus::Ahead),
                    _ => None,
                }
            },
        )
        .unwrap();

        assert_eq!(node_rev(&winner), "ahead");
    }

    #[test]
    fn auto_dedup_keeps_base_when_candidate_is_behind_despite_newer_timestamp() {
        let winner = choose_lock_observation(
            vec![(100, github_node("base")), (500, github_node("behind"))],
            |base, head| {
                match (node_rev(base), node_rev(head)) {
                    ("base", "behind") => Some(super::CompareStatus::Behind),
                    _ => None,
                }
            },
        )
        .unwrap();

        assert_eq!(node_rev(&winner), "base");
    }

    #[test]
    fn auto_dedup_falls_back_to_timestamp_for_diverged_histories() {
        let winner = choose_lock_observation(
            vec![(100, github_node("base")), (500, github_node("amended"))],
            |base, head| {
                match (node_rev(base), node_rev(head)) {
                    ("base", "amended") => Some(super::CompareStatus::Diverged),
                    _ => None,
                }
            },
        )
        .unwrap();

        assert_eq!(node_rev(&winner), "amended");
    }

    #[test]
    fn group_divergence_uses_full_revs_not_display_revs() {
        let entries = vec![
            entry_full(
                &[],
                "base",
                "abcdef0",
                "abcdef0000000000000000000000000000000000",
                Some(10),
            ),
            entry_full(
                &["dep"],
                "head",
                "abcdef0",
                "abcdef0999999999999999999999999999999999",
                Some(20),
            ),
        ];

        assert!(group_diverges(&entries));
    }

    #[test]
    fn compare_jobs_use_full_revs_and_display_short_keys() {
        let mut groups = BTreeMap::new();
        groups.insert("github:o/r".to_owned(), vec![
            entry_full(
                &[],
                "base",
                "1111111",
                "1111111111111111111111111111111111111111",
                Some(10),
            ),
            entry_full(
                &["dep"],
                "head",
                "2222222",
                "2222222222222222222222222222222222222222",
                Some(20),
            ),
        ]);

        let (jobs, capped) = compare_jobs(&groups);

        assert_eq!(capped, 0);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].base, "1111111111111111111111111111111111111111");
        assert_eq!(jobs[0].head, "2222222222222222222222222222222222222222");
    }

    #[test]
    fn compare_jobs_are_capped_before_network_work() {
        let mut entries = vec![entry_full(&[], "base", "base", "base-full", Some(0))];
        for i in 0..(MAX_COMPARE_JOBS + 5) {
            entries.push(entry_full(
                &["dep"],
                &format!("head-{i:03}"),
                &format!("h{i:06}"),
                &format!("head-full-{i:03}"),
                Some(u64::try_from(i).unwrap() + 1),
            ));
        }
        let mut groups = BTreeMap::new();
        groups.insert("github:o/r".to_owned(), entries);

        let (jobs, capped) = compare_jobs(&groups);

        assert_eq!(jobs.len(), MAX_COMPARE_JOBS);
        assert_eq!(capped, 5);
    }

    #[test]
    fn group_marks_prefer_branch_status_over_misleading_timestamps() {
        let entries = vec![
            entry(&[], "base", "base", Some(500)),
            entry(&["dep"], "head", "head", Some(100)),
        ];
        let compares = HashMap::from([(
            ("github:o/r".to_owned(), "head".to_owned()),
            super::CompareStatus::Ahead,
        )]);

        let (marks, width) = group_marks(
            "github:o/r",
            &entries,
            entries.iter().map(|entry| entry.rev.as_str()),
            &compares,
        );
        let mark = &marks["head"];

        assert_eq!(width, 1);
        assert_eq!(mark.1, 1);
        assert!(mark.0.contains('\u{2191}'));
        assert!(!mark.0.contains('~'));
    }

    #[test]
    fn group_marks_show_diverged_branch_status_without_marker() {
        let entries = vec![
            entry(&[], "base", "base", Some(100)),
            entry(&["dep"], "head", "head", Some(200)),
        ];
        let compares = HashMap::from([(
            ("github:o/r".to_owned(), "head".to_owned()),
            super::CompareStatus::Diverged,
        )]);

        let (marks, width) = group_marks(
            "github:o/r",
            &entries,
            entries.iter().map(|entry| entry.rev.as_str()),
            &compares,
        );
        let mark = &marks["head"];

        assert_eq!(width, 2);
        assert_eq!(mark.1, 2);
        assert!(mark.0.contains('\u{2191}'));
        assert!(mark.0.contains('\u{2193}'));
        assert!(!mark.0.contains('~'));
    }

    #[test]
    fn group_marks_distinguish_timestamp_fallback_with_marker() {
        let entries = vec![
            entry(&[], "base", "base", Some(100)),
            entry(&["dep"], "head", "head", Some(200)),
        ];
        let compares = HashMap::new();

        let (marks, width) = group_marks(
            "github:o/r",
            &entries,
            entries.iter().map(|entry| entry.rev.as_str()),
            &compares,
        );
        let mark = &marks["head"];

        assert_eq!(width, 2);
        assert_eq!(mark.1, 2);
        assert!(mark.0.contains('\u{2191}')); // ↑ newer by timestamp
        assert!(mark.0.contains('~')); // marked as a date-based guess
    }

    #[test]
    fn apply_follows_syncs_rev_full_rev_and_lm_to_target() {
        let mut groups = BTreeMap::new();
        groups.insert("github:o/r".to_owned(), vec![
            entry(&[], "nixpkgs", "newrev", Some(100)),
            // a transitive input that follows nixpkgs, carrying its own stale rev
            // and timestamp from before the follow was applied
            entry(&["dep"], "nixpkgs-lib", "oldrev", Some(50)),
        ]);
        let by_name = BTreeMap::new(); // top resolves via [all_follow], not a parent's follows
        let all_follow = map(&[("nixpkgs-lib", "nixpkgs")]);
        let top_revs = map(&[("nixpkgs", "newrev")]);
        let top_full_revs = map(&[("nixpkgs", "newrev-full")]);
        let top_lms = iter::once(("nixpkgs".to_owned(), 100_u64)).collect();

        apply_follows(
            &mut groups,
            &by_name,
            &all_follow,
            &top_revs,
            &top_full_revs,
            &top_lms,
        );

        let followed = &groups["github:o/r"][1];
        assert_eq!(followed.rev, "newrev");
        assert_eq!(followed.full_rev, "newrev-full");
        // lm should track the target rather than keeping the stale 50
        assert_eq!(followed.lm, Some(100));
    }

    #[test]
    fn apply_follows_honors_scoped_all_follow_per_side() {
        // an upstream tack pin `dep`, recorded as a tack-side finding
        let mut groups = BTreeMap::new();
        groups.insert("github:o/r".to_owned(), vec![tack_entry(
            &["parent"],
            "dep",
            "oldrev",
            Some(50),
        )]);
        let by_name = BTreeMap::new();
        let top_revs = map(&[("replacement", "newrev")]);
        let top_full_revs = map(&[("replacement", "newrev-full")]);
        let top_lms = iter::once(("replacement".to_owned(), 100_u64)).collect();

        // a `flake:`-scoped rule must not touch a tack-side entry
        let flake_rule = map(&[("flake:dep", "replacement")]);
        apply_follows(
            &mut groups,
            &by_name,
            &flake_rule,
            &top_revs,
            &top_full_revs,
            &top_lms,
        );
        assert_eq!(groups["github:o/r"][0].rev, "oldrev");

        // the matching `tack:`-scoped rule aligns it onto the target
        let tack_rule = map(&[("tack:dep", "replacement")]);
        apply_follows(
            &mut groups,
            &by_name,
            &tack_rule,
            &top_revs,
            &top_full_revs,
            &top_lms,
        );
        assert_eq!(groups["github:o/r"][0].rev, "newrev");
        assert_eq!(groups["github:o/r"][0].full_rev, "newrev-full");
        assert_eq!(groups["github:o/r"][0].lm, Some(100));
    }

    #[test]
    fn rm_removes_orphaned_lock_entry() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pins.toml"), "[inputs]\n").unwrap();
        fs::write(
            dir.path().join("pins.lock.json"),
            r#"{"gone":{"type":"github","owner":"o","repo":"r","rev":"bad","narHash":"sha256-x"}}"#,
        )
        .unwrap();

        assert_eq!(rm_in_dir(dir.path(), "gone").unwrap(), (false, true));
        assert_eq!(
            fs::read_to_string(dir.path().join("pins.toml")).unwrap(),
            "[inputs]\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("pins.lock.json")).unwrap(),
            "{}\n"
        );
    }

    #[test]
    fn rm_errors_when_pin_and_lock_are_missing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pins.toml"), "[inputs]\n").unwrap();
        fs::write(dir.path().join("pins.lock.json"), "{}\n").unwrap();

        let err = rm_in_dir(dir.path(), "missing").unwrap_err().to_string();
        assert_eq!(err, "no input 'missing'");
    }
}
