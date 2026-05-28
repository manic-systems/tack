// SPDX-License-Identifier: EUPL-1.2

use std::{
    env,
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

use crate::{
    fetch,
    lock,
    pins,
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
    let d = dir();
    let (pt, lp, rp) = (pins_path(&d), lock_path(&d), resolver_path(&d));

    if !force {
        let clash: Vec<String> = [&pt, &rp]
            .into_iter()
            .filter_map(|path| path.exists().then_some(path.display().to_string()))
            .collect::<Vec<String>>();
        if !clash.is_empty() {
            bail!("{} already exists (use --force)", clash.join(", "));
        }
    }
    std::fs::create_dir_all(&d)?;
    write_atomic(&pt, STARTER_TOML)?;
    if !lp.exists() {
        write_atomic(&lp, "{}\n")?;
    }
    write_atomic(&rp, RESOLVER_NIX)?;

    let resolver_name = rp
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("default.nix");
    let import_hint = if d.ends_with(".tack") {
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
    flake: bool,
    dir_field: Option<&str>,
    submodules: bool,
    follows: &[(String, String)],
) -> Result<()> {
    let dir = dir();
    let mut doc = pins::load(&pins_path(&dir))?;
    if pins::has_input(&doc, name) {
        bail!("input '{name}' already exists");
    }
    pins::add_input(&mut doc, name, url, flake, dir_field, submodules, follows);
    pins::save(&pins_path(&dir), &doc)?;

    let expanded = shorturl::expand(url, &pins::shorturls(&doc));
    match fetch::fetch_pin(&expanded, submodules) {
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
    refresh_resolver(&d);
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
    refresh_resolver(&d);
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
    refresh_resolver(&d);
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
            match fetch::fetch_pin(&expanded, inp.submodules) {
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
    refresh_resolver(&d);

    if drift.into_inner() > 0 {
        bail!(
            "locked rev unchanged but upstream content differs (lock kept; investigate before \
             relocking)"
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

pub fn help() {
    println!(
        "tack: flake-like toml nix pins, lazily fetched and transformed

usage:
  tack [-h|--help|help]
  tack init [--force]
  tack update [names...] [--accept]
  tack look [names...]
  tack add <name> <url> [--no-flake] [--dir <d>] [--submodules] [--follows c=p]...
  tack rm <name>
  tack alias <name> <template> | tack alias --rm <name>

tack lives in ./.tack/ by default; legacy root layouts (pins.toml etc. at
cwd alongside inputs.nix) are detected and kept as-is. TACK_DIR overrides.

import from your flake/config:

  outputs = { self }:
    let inputs = import ./.tack; in {
      packages.x86_64-linux.default =
        inputs.nixpkgs.legacyPackages.x86_64-linux.hello;
    }};

git flakes only see tracked files, so commit the contents of .tack/"
    );
}
