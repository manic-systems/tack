// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::BTreeMap,
    path::Path,
    process::Command,
};

use misstep::{
    Result,
    ResultExt as _,
    bail,
};
use serde::Deserialize;

use crate::{
    pins::{
        self,
        AddInputOpts,
        PinType,
        PinsDoc,
    },
    project::Project,
};

pub(super) fn convert(project: &Project, flake_path: &Path) -> Result<usize> {
    let inputs = eval_inputs(flake_path)?;
    if inputs.is_empty() {
        return Ok(0);
    }

    let mut doc = project.load_pins()?;
    let mut added = 0;
    let mut pending = Vec::new();
    for (name, input) in inputs {
        if pins::is_reserved(&name) {
            eprintln!("tack: input '{name}' is reserved by tack, skipped");
            continue;
        }
        if !pins::is_plain_name(&name) {
            eprintln!("tack: input '{name}' can't be a pin name, skipped");
            continue;
        }
        if doc.has_input(&name) {
            eprintln!("tack: input '{name}' already in pins.toml, skipped");
            continue;
        }
        let Some(url) = input.url.as_deref() else {
            match input.follows {
                Some(target) if !target.is_empty() => pending.push((name, target)),
                _ => eprintln!("tack: input '{name}' has no url, skipped"),
            }
            continue;
        };
        let follows = input.follows();
        doc.add_input(&name, url, &AddInputOpts {
            pin_type:   input.pin_type(),
            unpack:     None,
            dir:        input.dir.as_deref(),
            submodules: input.submodules.unwrap_or(false),
            follows:    &follows,
            tag:        None,
            tags_from:  None,
        })?;
        added += 1;
    }
    added += add_top_follows(&mut doc, pending)?;
    project.save_pins(&doc)?;
    Ok(added)
}

/// a top-level follow can name another, so each pass keeps the ones that
/// resolve until a pass adds nothing
fn add_top_follows(doc: &mut PinsDoc, mut pending: Vec<(String, String)>) -> Result<usize> {
    let mut added = 0;
    loop {
        let before = pending.len();
        let mut unresolved = Vec::new();
        for (name, target) in pending {
            doc.add_top_follow(&name, &target)?;
            if doc.top_follows().is_ok() {
                added += 1;
            } else {
                doc.remove_input(&name);
                unresolved.push((name, target));
            }
        }
        pending = unresolved;
        if pending.len() == before {
            break;
        }
    }
    for (name, target) in pending {
        eprintln!("tack: input '{name}' follows '{target}', which doesn't resolve, skipped");
    }
    Ok(added)
}

fn eval_inputs(flake_path: &Path) -> Result<BTreeMap<String, FlakeInput>> {
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "eval",
            "--file",
        ])
        .arg(flake_path)
        .args(["--apply", "flake: flake.inputs or { }", "--json"])
        .output()
        .context("run `nix eval` (is nix installed?)")?;
    if !out.status.success() {
        bail!(
            "nix eval failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout)
        .context("parse `nix eval` json")
        .erase()
}

#[derive(Deserialize)]
struct FlakeInput {
    url:        Option<String>,
    flake:      Option<bool>,
    dir:        Option<String>,
    submodules: Option<bool>,
    follows:    Option<String>,
    #[serde(default)]
    inputs:     BTreeMap<String, Override>,
}

#[derive(Deserialize)]
struct Override {
    follows: Option<String>,
}

impl FlakeInput {
    fn pin_type(&self) -> PinType {
        if self.flake == Some(false) {
            PinType::Fetch
        } else {
            PinType::Flake
        }
    }

    fn follows(&self) -> Vec<(String, String)> {
        self.inputs
            .iter()
            .filter_map(|(child, over)| Some((child.clone(), over.follows.clone()?)))
            .collect::<Vec<_>>()
    }
}
