// SPDX-License-Identifier: EUPL-1.2

use anyhow::{Result, bail};
use rayon::prelude::*;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::ui::{Display, PinStatus};
use crate::{fetch, lock, pins, shorturl};

const STARTER_TOML: &str = include_str!("../assets/pins.toml");
const RESOLVER_NIX: &str = include_str!("../inputs.nix");

fn dir() -> PathBuf {
    std::env::var_os("TACK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"))
}

fn pins_path(d: &Path) -> PathBuf {
    d.join("pins.toml")
}
fn lock_path(d: &Path) -> PathBuf {
    d.join("pins.lock.json")
}

fn short(rev: &str) -> String {
    fn trim(s: &str) -> &str {
        let s = s.split('?').next().unwrap_or(s);
        s.split('#').next().unwrap_or(s)
    }
    if rev.contains("://") {
        let segs = rev
            .split_once("://")
            .map(|x| x.1)
            .unwrap_or("")
            .split('/')
            .filter(|s| !s.is_empty())
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
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn init(force: bool) -> Result<()> {
    let d = dir();
    let (pt, lp, ip) = (pins_path(&d), lock_path(&d), d.join("inputs.nix"));

    if !force {
        let clash: Vec<String> = [&pt, &ip]
            .into_iter()
            .filter(|p| p.exists())
            .map(|p| p.display().to_string())
            .collect();
        if !clash.is_empty() {
            bail!("{} already exists (use --force)", clash.join(", "));
        }
    }
    write_atomic(&pt, STARTER_TOML)?;
    if !lp.exists() {
        write_atomic(&lp, "{}\n")?;
    }
    write_atomic(&ip, RESOLVER_NIX)?;

    println!("initialised tack in {}", d.display());
    println!("  pins.toml       edit shorturls and inputs here");
    println!("  pins.lock.json  written by `tack update`");
    println!("  inputs.nix      `import ./inputs.nix` from your flake/config");
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
    let d = dir();
    let mut doc = pins::load(&pins_path(&d))?;
    if pins::has_input(&doc, name) {
        bail!("input '{name}' already exists");
    }
    pins::add_input(&mut doc, name, url, flake, dir_field, submodules, follows);
    pins::save(&pins_path(&d), &doc)?;

    let expanded = shorturl::expand(url, &pins::shorturls(&doc));
    match fetch::fetch_pin(&expanded, submodules) {
        Ok((node, rev)) => {
            let mut lk = lock::load(&lock_path(&d))?;
            lk.insert(name.to_string(), node);
            lock::save(&lock_path(&d), &lk)?;
            println!("added {name}  NEW -> {}", short(&rev));
        }
        Err(e) => {
            println!("added {name} to pins.toml, but locking failed: {e:#}");
            println!("  fix the url and run `tack update {name}`");
        }
    }
    Ok(())
}

pub fn rm(name: &str) -> Result<()> {
    let d = dir();
    let mut doc = pins::load(&pins_path(&d))?;
    if !pins::remove_input(&mut doc, name) {
        bail!("no input '{name}'");
    }
    pins::save(&pins_path(&d), &doc)?;

    let mut lk = lock::load(&lock_path(&d))?;
    lk.remove(name);
    lock::save(&lock_path(&d), &lk)?;
    println!("removed {name}");
    Ok(())
}

pub fn alias(name: &str, template: Option<&str>, remove: bool) -> Result<()> {
    let d = dir();
    let mut doc = pins::load(&pins_path(&d))?;
    if remove {
        if !pins::remove_alias(&mut doc, name) {
            bail!("no alias '{name}'");
        }
        pins::save(&pins_path(&d), &doc)?;
        println!("removed alias {name}");
    } else {
        let template = template.expect("template required");
        if !template.contains("{path}") {
            bail!("alias template must contain '{{path}}'");
        }
        pins::set_alias(&mut doc, name, template);
        pins::save(&pins_path(&d), &doc)?;
        println!("alias {name} = {template}");
    }
    Ok(())
}

pub fn update(names: &[String], accept: bool) -> Result<()> {
    let d = dir();
    let doc = pins::load(&pins_path(&d))?;
    let shorturls = pins::shorturls(&doc);
    let all = pins::inputs(&doc)?;
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(());
    }
    let mut lk = lock::load(&lock_path(&d))?;

    let display = Display::new(selected.iter().map(|i| i.name.clone()).collect());
    let drift = AtomicUsize::new(0);

    let results: Vec<Option<Value>> = selected
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
                        (Some(o), Some(n)) if o != n
                    );
                    match (drifted, accept) {
                        // relock to the drifted tree, the user vouched for it
                        (true, true) => {
                            display.set(
                                i,
                                PinStatus::Drift {
                                    rev: short(&rev),
                                    accepted: true,
                                },
                            );
                            Some(node)
                        }
                        // trip the alarm and keep the locked node
                        (true, false) => {
                            drift.fetch_add(1, Ordering::Relaxed);
                            display.set(
                                i,
                                PinStatus::Drift {
                                    rev: short(&rev),
                                    accepted: false,
                                },
                            );
                            None
                        }
                        (false, _) => {
                            display.set(i, PinStatus::NoChange);
                            None
                        }
                    }
                }
                Ok((node, rev)) => {
                    display.set(
                        i,
                        PinStatus::Updated {
                            old: old_rev.map(short).unwrap_or_else(|| "NEW".into()),
                            new: short(&rev),
                        },
                    );
                    Some(node)
                }
                Err(e) => {
                    display.set(i, PinStatus::Failed(format!("{e:#}")));
                    None
                }
            }
        })
        .collect();

    let mut changed = false;
    for (inp, node) in selected.iter().zip(results) {
        if let Some(node) = node {
            lk.insert(inp.name.clone(), node);
            changed = true;
        }
    }
    if changed {
        lock::save(&lock_path(&d), &lk)?;
    }
    display.finish();

    if drift.into_inner() > 0 {
        bail!(
            "locked rev unchanged but upstream content differs (lock kept; investigate before relocking)"
        );
    }
    Ok(())
}

pub fn look(names: &[String]) -> Result<()> {
    let d = dir();
    let doc = pins::load(&pins_path(&d))?;
    let shorturls = pins::shorturls(&doc);
    let all = pins::inputs(&doc)?;
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(());
    }
    let lk = lock::load(&lock_path(&d))?;

    let display = Display::new(selected.iter().map(|i| i.name.clone()).collect());

    selected.par_iter().enumerate().for_each(|(i, inp)| {
        display.set(i, PinStatus::Fetching);
        let expanded = shorturl::expand(&inp.url, &shorturls);
        let old = lk.get(&inp.name).and_then(lock::rev_of).map(str::to_string);
        match fetch::current_rev(&expanded) {
            Ok(rev) if old.as_deref() == Some(rev.as_str()) => {
                display.set(i, PinStatus::NoChange);
            }
            Ok(rev) => display.set(
                i,
                PinStatus::Updated {
                    old: old.as_deref().map(short).unwrap_or_else(|| "NEW".into()),
                    new: short(&rev),
                },
            ),
            Err(e) => display.set(i, PinStatus::Failed(format!("{e:#}"))),
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
        "{}",
        "tack: flake-like toml nix pins, lazily fetched and transformed

usage:
  tack [-h|--help|help]
  tack init [--force]
  tack update [names...] [--accept]
  tack look [names...]
  tack add <name> <url> [--no-flake] [--dir <d>] [--submodules] [--follows c=p]...
  tack rm <name>
  tack alias <name> <template> | tack alias --rm <name>

import inputs.nix from your flake/config:

  outputs = { self }:
    let inputs = import ./inputs.nix; in {
      packages.x86_64-linux.default =
        inputs.nixpkgs.legacyPackages.x86_64-linux.hello;
    };

git flakes only see tracked files, so commit pins.toml, pins.lock.json and
inputs.nix"
    );
}
