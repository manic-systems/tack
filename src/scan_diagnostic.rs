// SPDX-License-Identifier: EUPL-1.2

use std::fmt::{
    self,
    Display,
    Formatter,
};

use crate::render::printable;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ScanDiagnostic {
    path: Vec<String>,
    file: ScanFile,
    kind: ScanDiagnosticKind,
}

impl ScanDiagnostic {
    pub fn fetch<E: Display>(path: &[String], file: ScanFile, error: E) -> Self {
        Self::new(path, file, ScanDiagnosticKind::Fetch(format!("{error:#}")))
    }

    pub fn private_repo(path: &[String], file: ScanFile, base: &str) -> Self {
        Self::new(
            path,
            file,
            ScanDiagnosticKind::Fetch(format!(
                "{} not readable anonymously, the repository may be private (set GITHUB_TOKEN or \
                 GH_TOKEN, GITLAB_TOKEN for gitlab, or TACK_NIX_CONF_TOKENS=1)",
                printable(base)
            )),
        )
    }

    pub fn parse<E: Display>(path: &[String], file: ScanFile, error: E) -> Self {
        Self::new(path, file, ScanDiagnosticKind::Parse(format!("{error:#}")))
    }

    pub fn config<E: Display>(path: &[String], file: ScanFile, error: E) -> Self {
        Self::new(path, file, ScanDiagnosticKind::Config(format!("{error:#}")))
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
}

impl Display for ScanDiagnosticKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Fetch(ref error) => write!(f, "fetch failed: {error}"),
            Self::Parse(ref error) => write!(f, "parse failed: {error}"),
            Self::Config(ref error) => write!(f, "config invalid: {error}"),
        }
    }
}
