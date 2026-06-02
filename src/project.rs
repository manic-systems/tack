// SPDX-License-Identifier: EUPL-1.2

use std::{
    env,
    fs,
    io,
    path::{
        Path,
        PathBuf,
    },
    result::Result as StdResult,
};

use eyre::Result as EyreResult;

use crate::{
    lock,
    pins,
};

#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("no pins.toml at {0} (run \u{60}tack init\u{60})")]
    Missing(PathBuf),
    #[error("read {path}")]
    Read {
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("parse {path}")]
    ParseToml {
        path:   PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
    #[error("parse {path}")]
    ParseLock {
        path:   PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("read current directory")]
    CurrentDir {
        #[source]
        source: io::Error,
    },
}

/// on-disk tack workspace
pub struct Project {
    dir: PathBuf,
}

impl Project {
    /// `$TACK_DIR`, else cwd with legacy `inputs.nix`, else `cwd/.tack`
    pub fn discover() -> StdResult<Self, ConfigError> {
        if let Some(dir) = env::var_os("TACK_DIR") {
            return Ok(Self::at(PathBuf::from(dir)));
        }
        let cwd = env::current_dir().map_err(|source| ConfigError::CurrentDir { source })?;
        if cwd.join("inputs.nix").exists() {
            return Ok(Self::at(cwd));
        }
        Ok(Self::at(cwd.join(".tack")))
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

    /// `inputs.nix` for the legacy repo-root layout, else `default.nix`
    pub fn resolver_path(&self) -> PathBuf {
        let legacy = self.dir.join("inputs.nix");
        if legacy.exists() {
            return legacy;
        }
        self.dir.join("default.nix")
    }

    pub fn load_pins(&self) -> StdResult<pins::PinsDoc, ConfigError> {
        let path = self.pins_path();
        if !path.exists() {
            return Err(ConfigError::Missing(path));
        }
        let raw = fs::read_to_string(&path).map_err(|source| {
            ConfigError::Read {
                path: path.clone(),
                source,
            }
        })?;
        pins::PinsDoc::parse(&raw).map_err(|source| ConfigError::ParseToml { path, source })
    }

    pub fn save_pins(&self, doc: &pins::PinsDoc) -> EyreResult<()> {
        doc.save(&self.pins_path())
    }

    pub fn load_lock(&self) -> StdResult<lock::LockFile, ConfigError> {
        let path = self.lock_path();
        if !path.exists() {
            return Ok(lock::LockFile::new());
        }
        let raw = fs::read_to_string(&path).map_err(|source| {
            ConfigError::Read {
                path: path.clone(),
                source,
            }
        })?;
        let lock = lock::parse(&raw).map_err(|source| {
            ConfigError::ParseLock {
                path: path.clone(),
                source,
            }
        })?;
        for name in lock.unknown_nodes() {
            eprintln!(
                "tack: skipping unrecognized lock entry '{name}' in {} (kept as-is)",
                path.display()
            );
        }
        Ok(lock)
    }

    pub fn save_lock(&self, lk: &lock::LockFile) -> EyreResult<()> {
        lk.save(&self.lock_path())
    }
}

/// atomic write via sibling temp + rename
pub fn write_atomic(path: &Path, contents: &str) -> EyreResult<()> {
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

    use super::{
        ConfigError,
        Project,
    };

    #[test]
    fn modern_layout_paths_hang_off_dir() {
        let project = Project::at("/work/.tack".into());
        assert!(project.pins_path().ends_with(".tack/pins.toml"));
        assert!(project.lock_path().ends_with(".tack/pins.lock.json"));
        assert!(project.resolver_path().ends_with("default.nix"));
    }

    #[test]
    fn missing_pins_is_a_missing_config_error() {
        let project = Project::at("/definitely/not/here".into());
        let err = project.load_pins().unwrap_err();

        assert!(matches!(err, ConfigError::Missing(_)));
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
