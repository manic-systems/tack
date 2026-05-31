// SPDX-License-Identifier: EUPL-1.2

use std::{
    env,
    fs,
    path::{
        Path,
        PathBuf,
    },
};

use anyhow::Result;
use toml_edit::DocumentMut;

use crate::{
    lock,
    pins,
};

/// The on-disk tack workspace: a directory and the files it owns.
pub struct Project {
    dir: PathBuf,
}

impl Project {
    /// Discover the workspace: `$TACK_DIR`, else cwd when it carries the legacy
    /// `inputs.nix`, else `cwd/.tack`.
    pub fn discover() -> Self {
        if let Some(dir) = env::var_os("TACK_DIR") {
            return Self::at(PathBuf::from(dir));
        }
        let cwd = env::current_dir().expect("cwd");
        if cwd.join("inputs.nix").exists() {
            return Self::at(cwd);
        }
        Self::at(cwd.join(".tack"))
    }

    pub const fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn pins_path(&self) -> PathBuf {
        self.dir.join("pins.toml")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("pins.lock.json")
    }

    /// `inputs.nix` for the legacy repo-root layout, else `default.nix`.
    pub fn resolver_path(&self) -> PathBuf {
        let legacy = self.dir.join("inputs.nix");
        if legacy.exists() {
            return legacy;
        }
        self.dir.join("default.nix")
    }

    pub fn load_pins(&self) -> Result<DocumentMut> {
        pins::load(&self.pins_path())
    }

    pub fn save_pins(&self, doc: &DocumentMut) -> Result<()> {
        pins::save(&self.pins_path(), doc)
    }

    pub fn load_lock(&self) -> Result<lock::Lock> {
        lock::load(&self.lock_path())
    }

    pub fn save_lock(&self, lk: &lock::Lock) -> Result<()> {
        lock::save(&self.lock_path(), lk)
    }
}

/// Write `contents` to `path` atomically via a sibling temp + rename.
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let mut tmp_str = path.as_os_str().to_owned();
    tmp_str.push(".tmp");
    let tmp = PathBuf::from(tmp_str);
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::Project;

    #[test]
    fn modern_layout_paths_hang_off_dir() {
        let project = Project::at("/work/.tack".into());
        assert!(project.pins_path().ends_with(".tack/pins.toml"));
        assert!(project.lock_path().ends_with(".tack/pins.lock.json"));
        assert!(project.resolver_path().ends_with("default.nix"));
    }

    #[test]
    fn legacy_layout_uses_inputs_nix_resolver() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("inputs.nix"), "").unwrap();
        let project = Project::at(tmp.path().to_path_buf());

        assert_eq!(project.pins_path(), tmp.path().join("pins.toml"));
        assert_eq!(project.lock_path(), tmp.path().join("pins.lock.json"));
        assert_eq!(project.resolver_path(), tmp.path().join("inputs.nix"));
    }
}
