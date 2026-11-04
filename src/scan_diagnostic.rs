// SPDX-License-Identifier: EUPL-1.2

use std::fmt::{
    self,
    Display,
    Formatter,
};

use crate::fetch;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ScanDiagnostic {
    path: Vec<String>,
    file: ScanFile,
    kind: ScanDiagnosticKind,
}

impl ScanDiagnostic {
    pub fn fetch<E>(path: &[String], file: ScanFile, error: E) -> Self
    where
        E: Display,
    {
        Self::new(path, file, ScanDiagnosticKind::Fetch(format!("{error:#}")))
    }

    pub fn private_repo(path: &[String], file: ScanFile, base: &str) -> Self {
        Self::new(
            path,
            file,
            ScanDiagnosticKind::Fetch(fetch::private_repo_hint(base)),
        )
    }

    pub fn parse<E>(path: &[String], file: ScanFile, error: E) -> Self
    where
        E: Display,
    {
        Self::new(path, file, ScanDiagnosticKind::Parse(format!("{error:#}")))
    }

    pub fn config<E>(path: &[String], file: ScanFile, error: E) -> Self
    where
        E: Display,
    {
        Self::new(path, file, ScanDiagnosticKind::Config(format!("{error:#}")))
    }

    pub fn truncated(path: &[String], file: ScanFile, limit: String) -> Self {
        Self::new(path, file, ScanDiagnosticKind::Truncated(limit))
    }

    pub fn path(&self) -> &[String] {
        &self.path
    }

    pub const fn file(&self) -> ScanFile {
        self.file
    }

    pub(super) const fn kind(&self) -> &ScanDiagnosticKind {
        &self.kind
    }

    fn new(path: &[String], file: ScanFile, kind: ScanDiagnosticKind) -> Self {
        Self {
            path: path.to_vec(),
            file,
            kind,
        }
    }
}

impl Display for ScanDiagnostic {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.file(), self.kind())
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ScanFile {
    FlakeLock,
    TackPins,
    TackLock,
}

impl ScanFile {
    pub const fn as_path(self) -> &'static str {
        match self {
            Self::FlakeLock => "flake.lock",
            Self::TackPins => ".tack/pins.toml",
            Self::TackLock => ".tack/pins.lock.json",
        }
    }
}

impl Display for ScanFile {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_path())
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ScanDiagnosticKind {
    Fetch(String),
    Parse(String),
    Config(String),
    /// a scan cut short by one of dedup's size limits
    Truncated(String),
}

impl Display for ScanDiagnosticKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Fetch(ref error) => write!(f, "fetch failed: {error}"),
            Self::Parse(ref error) => write!(f, "parse failed: {error}"),
            Self::Config(ref error) => write!(f, "config invalid: {error}"),
            Self::Truncated(ref limit) => write!(f, "truncated: {limit}"),
        }
    }
}
