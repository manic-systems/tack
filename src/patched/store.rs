// SPDX-License-Identifier: EUPL-1.2

use std::{
    fs,
    path::Path,
    process::Command,
};

use misstep::{
    Result,
    ResultExt as _,
    bail,
};
use serde::{
    Deserialize,
    Serialize,
};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct StorePath(String);

impl StorePath {
    pub fn add(tree: &Path) -> Result<Self> {
        Self::add_fixed(&["--recursive"], tree)
    }

    pub fn add_flat(file: &Path) -> Result<Self> {
        Self::add_fixed(&[], file)
    }

    fn add_fixed(mode: &[&str], path: &Path) -> Result<Self> {
        let mut command = Command::new("nix-store");
        command
            .arg("--add-fixed")
            .args(mode)
            .arg("sha256")
            .arg(path);
        let stored = stdout_of(&mut command)?;
        if !Path::new(&stored).is_absolute() {
            bail!("nix-store --add-fixed printed '{stored}' instead of a store path");
        }
        Ok(Self(stored))
    }

    pub fn root_at(&self, link: &Path) -> Result<()> {
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut command = Command::new("nix-store");
        command.args(["--realise", &self.0, "--add-root"]).arg(link);
        stdout_of(&mut command).map(drop)
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    pub fn exists(&self) -> bool {
        Path::new(&self.0).exists()
    }

    /// the narHash nix's database holds for the path, [`None`] when it can't
    /// say, since `nix path-info` prints a map or a list depending on the
    /// version and older ones print base32
    pub fn recorded_nar_hash(&self) -> Option<String> {
        let mut command = Command::new("nix");
        command.args(["path-info", "--json"]).arg(&self.0);
        let printed = stdout_of(&mut command).ok()?;
        let parsed = serde_json::from_str::<Value>(&printed).ok()?;
        let info = match parsed {
            Value::Object(map) => map.into_iter().next()?.1,
            Value::Array(list) => list.into_iter().next()?,
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => return None,
        };
        let hash = info.get("narHash")?.as_str()?;
        hash.starts_with("sha256-").then(|| hash.to_owned())
    }
}

fn stdout_of(command: &mut Command) -> Result<String> {
    let output = command
        .output()
        .with_context(|| format!("run {command:?}"))?;
    if !output.status.success() {
        bail!(
            "{command:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
