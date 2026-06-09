// SPDX-License-Identifier: EUPL-1.2

//! import a flake.nix's `inputs` attr into pins.toml

use std::{
    collections::BTreeMap,
    path::Path,
    process::Command,
};

use eyre::{
    Result,
    WrapErr as _,
    bail,
};
use serde::Deserialize;

use crate::{
    pins::{
        AddInputOpts,
        PinType,
    },
    project::Project,
};

pub fn convert(project: &Project, flake_path: &Path) -> Result<usize> {
    let inputs = eval_inputs(flake_path)?;
    if inputs.is_empty() {
        return Ok(0);
    }

    let mut doc = project.load_pins()?;
    let mut added = 0;
    for (name, input) in inputs {
        if doc.has_input(&name) {
            eprintln!("tack: input '{name}' already in pins.toml, skipped");
            continue;
        }
        let Some(url) = input.url.as_deref() else {
            eprintln!("tack: input '{name}' has no url, skipped");
            continue;
        };
        let follows = input.follows();
        doc.add_input(&name, url, &AddInputOpts {
            pin_type:   input.pin_type(),
            unpack:     None,
            dir:        input.dir.as_deref(),
            submodules: input.submodules.unwrap_or(false),
            follows:    &follows,
        });
        added += 1;
    }
    project.save_pins(&doc)?;
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
        .wrap_err("run `nix eval` (is nix installed?)")?;
    if !out.status.success() {
        bail!(
            "nix eval failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).wrap_err("parse `nix eval` json")
}

#[derive(Deserialize)]
struct FlakeInput {
    url:        Option<String>,
    flake:      Option<bool>,
    dir:        Option<String>,
    submodules: Option<bool>,
    /// nested `inputs.<child>` overrides; we keep the `.follows` ones
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

#[cfg(test)]
mod tests {
    use super::FlakeInput;
    use crate::pins::PinType;

    fn input(json: &str) -> FlakeInput {
        serde_json::from_str(json).expect("deserialize")
    }

    #[test]
    fn flake_false_becomes_a_fetch_pin() {
        assert_eq!(
            input(r#"{ "url": "github:o/r", "flake": false }"#).pin_type(),
            PinType::Fetch
        );
        assert_eq!(
            input(r#"{ "url": "github:o/r" }"#).pin_type(),
            PinType::Flake
        );
    }

    #[test]
    fn follows_are_lifted_from_nested_inputs() {
        let parsed = input(
            r#"{
              "url": "github:ipetkov/crane",
              "inputs": {
                "nixpkgs": { "follows": "nixpkgs" },
                "flake-utils": { "follows": "flake-utils" },
                "other": { "url": "github:o/other" }
              }
            }"#,
        );
        assert_eq!(parsed.follows(), vec![
            ("flake-utils".to_owned(), "flake-utils".to_owned()),
            ("nixpkgs".to_owned(), "nixpkgs".to_owned()),
        ]);
    }
}
