// SPDX-License-Identifier: EUPL-1.2

use std::{
    env,
    fs,
    io,
    path::{
        Path,
        PathBuf,
    },
    process,
    result::Result as StdResult,
    time::{
        SystemTime,
        UNIX_EPOCH,
    },
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

pub struct Project {
    dir: PathBuf,
}

impl Project {
    /// `$TACK_DIR`, the nearest enclosing project, or `cwd/.tack` when there is
    /// none
    pub fn discover() -> StdResult<Self, ConfigError> {
        if let Some(dir) = env::var_os("TACK_DIR") {
            return Ok(Self::at(PathBuf::from(dir)));
        }
        let cwd = Self::cwd()?;
        Ok(cwd
            .ancestors()
            .find_map(Self::rooted_at)
            .unwrap_or_else(|| Self::at(cwd.join(".tack"))))
    }

    /// `$TACK_DIR` legacy cwd or `cwd/.tack`, never an enclosing project
    ///
    /// `init` scaffolds where it was invoked, so searching upward would let it
    /// overwrite the parent project of whatever subdirectory you happen to be
    /// in.
    pub fn here() -> StdResult<Self, ConfigError> {
        if let Some(dir) = env::var_os("TACK_DIR") {
            return Ok(Self::at(PathBuf::from(dir)));
        }
        let cwd = Self::cwd()?;
        if cwd.join("pins.toml").exists() || cwd.join("inputs.nix").exists() {
            return Ok(Self::at(cwd));
        }
        Ok(Self::at(cwd.join(".tack")))
    }

    fn cwd() -> StdResult<PathBuf, ConfigError> {
        env::current_dir().map_err(|source| ConfigError::CurrentDir { source })
    }

    /// a directory roots a project when it holds the pins itself or carries a
    /// `.tack`
    fn rooted_at(dir: &Path) -> Option<Self> {
        if dir.join("pins.toml").exists() || dir.join("inputs.nix").exists() {
            return Some(Self::at(dir.to_owned()));
        }
        let nested = dir.join(".tack");
        nested.is_dir().then(|| Self::at(nested))
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

    /// legacy `inputs.nix` else `default.nix`
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
        let lock = lock::LockFile::parse(&raw).map_err(|source| {
            ConfigError::ParseLock {
                path: path.clone(),
                source,
            }
        })?;
        let known_types = [
            "github", "gitlab", "git", "tarball", "fixed", "indirect", "path",
        ];
        for (name, value) in lock.unknown_nodes_with_values() {
            let tag = value.get("type").and_then(|val| val.as_str());
            let label = if tag.is_some_and(|kind| known_types.contains(&kind)) {
                "malformed"
            } else {
                "unrecognized"
            };
            eprintln!(
                "tack: skipping {label} lock entry '{name}' in {} (kept as-is)",
                path.display()
            );
        }
        Ok(lock)
    }

    pub fn save_lock(&self, lk: &lock::LockFile) -> EyreResult<()> {
        lk.save(&self.lock_path())
    }
}

pub fn write_atomic(path: &Path, contents: &str) -> EyreResult<()> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|dur| dur.as_nanos())
        .unwrap_or_default();
    let mut tmp_str = path.as_os_str().to_owned();
    tmp_str.push(format!(".{}.{}.tmp", process::id(), nanos));
    let tmp = PathBuf::from(tmp_str);
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
