// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::BTreeSet,
    fs,
    path::{
        Path,
        PathBuf,
    },
};

use eyre::Result;

use super::{
    super::tolerate,
    model::{
        Entry,
        Side,
    },
};
use crate::{
    fetch::{
        self,
        http::FetchError,
    },
    lock::{
        self,
        LockedNode,
    },
    pins::{
        self,
        PinType,
    },
    scan_diagnostic::{
        ScanDiagnostic,
        ScanFile,
    },
    source::{
        Source,
        forge::Forge,
        id::SourceId,
    },
};

pub(super) struct Finding {
    pub identity: SourceId,
    pub entry:    Entry,
}

pub(super) struct ScanResult {
    pub findings:    Vec<Finding>,
    pub transitive:  Vec<ScanTarget>,
    pub diagnostics: Vec<ScanDiagnostic>,
}

pub(super) struct ScanTarget {
    pub path:       Vec<String>,
    pub source:     SourceRef,
    pub submodules: bool,
}

impl ScanTarget {
    pub(super) fn fetch_and_scan(&self) -> Result<ScanResult> {
        if let SourceRef::Locked(ref node) = self.source
            && let Some((documents, diagnostics)) =
                RawProbe::documents(node, &self.path).into_documents()
        {
            let mut result = documents.scan(&self.path);
            result.diagnostics.extend(diagnostics);
            return Ok(result);
        }

        let tmp = tempfile::tempdir()?;
        let root = self.fetch_tree(tmp.path())?;
        Ok(ScanDocuments::from_tree(&root).scan(&self.path))
    }

    fn fetch_tree(&self, dir: &Path) -> Result<PathBuf> {
        match self.source {
            SourceRef::Locked(ref node) => fetch::fetch_locked_tree_into(node, dir),
            SourceRef::Url(ref url) => {
                let source = url.parse::<Source>()?;
                fetch::fetch_tree_into(&source, self.submodules, dir)
            },
        }
    }
}

pub(super) enum SourceRef {
    Locked(LockedNode),
    Url(String),
}

impl SourceRef {
    pub(super) fn key(&self) -> String {
        match *self {
            Self::Locked(ref node) => {
                // Transitive dependencies can differ across revisions of the
                // same source, so locked scan keys must include full_rev.
                let rev = node.full_rev().unwrap_or("");
                SourceId::from_locked(node).map_or_else(
                    || Self::tagged_key("locked", &[node.kind(), rev]),
                    |source_id| {
                        let identity = source_id.to_string();
                        Self::tagged_key("locked", &[&identity, rev])
                    },
                )
            },
            Self::Url(ref url) => Self::tagged_key("url", &[url]),
        }
    }

    fn tagged_key(tag: &str, parts: &[&str]) -> String {
        let mut key = tag.to_owned();
        for part in parts {
            key.push(':');
            key.push_str(&part.len().to_string());
            key.push(':');
            key.push_str(part);
        }
        key
    }
}

struct ScanDocuments {
    flake_lock: Option<String>,
    tack_pins:  Option<String>,
    tack_lock:  Option<String>,
}

struct RawProbeOutcome {
    documents:   Option<ScanDocuments>,
    diagnostics: BTreeSet<ScanDiagnostic>,
}

impl RawProbeOutcome {
    const fn empty() -> Self {
        Self {
            documents:   None,
            diagnostics: BTreeSet::new(),
        }
    }

    fn into_documents(self) -> Option<(ScanDocuments, Vec<ScanDiagnostic>)> {
        let documents = self.documents?;
        Some((documents, self.diagnostics.into_iter().collect()))
    }
}

struct RawProbe<'a> {
    forge: Forge,
    rev:   &'a str,
}

impl<'a> RawProbe<'a> {
    fn from_locked(node: &'a LockedNode) -> Option<Self> {
        Some(Self {
            forge: Forge::from_locked(node)?,
            rev:   node.rev()?,
        })
    }

    /// authoritative probes treat all-missing files as a real empty result
    /// non-authoritative probes fall back to cloning
    fn documents(node: &'a LockedNode, path: &[String]) -> RawProbeOutcome {
        let Some(probe) = Self::from_locked(node) else {
            return RawProbeOutcome::empty();
        };
        probe.probe_documents(path)
    }

    fn probe_documents(&self, path: &[String]) -> RawProbeOutcome {
        let mut diagnostics = BTreeSet::new();
        let mut probe = |file| {
            let (value, maybe_cause) = tolerate(self.fetch(file));
            if let Some(cause) = maybe_cause {
                diagnostics.insert(ScanDiagnostic::fetch(path, file, cause));
            }
            value
        };
        let documents = ScanDocuments {
            flake_lock: probe(ScanFile::FlakeLock),
            tack_pins:  probe(ScanFile::TackPins),
            tack_lock:  probe(ScanFile::TackLock),
        };
        if !self.forge.authoritative()
            && documents.flake_lock.is_none()
            && documents.tack_pins.is_none()
            && documents.tack_lock.is_none()
        {
            RawProbeOutcome {
                documents: None,
                diagnostics,
            }
        } else {
            RawProbeOutcome {
                documents: Some(documents),
                diagnostics,
            }
        }
    }

    fn fetch(&self, file: ScanFile) -> Result<String, FetchError> {
        let raw = self.forge.raw_file_url(self.rev, file.as_path());
        let body = fetch::raw(&raw.url)?;
        match raw.decoder {
            Some(decode) => {
                decode(&body).map_err(|source| {
                    FetchError::Decode {
                        what: file.as_path().to_owned(),
                        source,
                    }
                })
            },
            None => Ok(body),
        }
    }
}

impl ScanDocuments {
    fn from_tree(root: &Path) -> Self {
        let flake_lock = fs::read_to_string(root.join("flake.lock")).ok();
        let td = root.join(".tack");
        Self {
            flake_lock,
            tack_pins: fs::read_to_string(td.join("pins.toml")).ok(),
            tack_lock: fs::read_to_string(td.join("pins.lock.json")).ok(),
        }
    }

    fn scan(&self, path: &[String]) -> ScanResult {
        let mut findings = Vec::<Finding>::new();
        let mut transitive = Vec::<ScanTarget>::new();
        let mut diagnostics = Vec::<ScanDiagnostic>::new();

        self.scan_flake_lock(path, &mut findings, &mut diagnostics);
        self.scan_tack_inputs(path, &mut findings, &mut transitive, &mut diagnostics);

        ScanResult {
            findings,
            transitive,
            diagnostics,
        }
    }

    fn scan_flake_lock(
        &self,
        path: &[String],
        findings: &mut Vec<Finding>,
        diagnostics: &mut Vec<ScanDiagnostic>,
    ) {
        let Some(raw) = self.flake_lock.as_deref() else {
            return;
        };
        let doc = match lock::FlakeLock::parse(raw) {
            Ok(doc) => doc,
            Err(err) => {
                diagnostics.push(ScanDiagnostic::parse(path, ScanFile::FlakeLock, err));
                return;
            },
        };
        for (key, locked) in doc.locked_nodes() {
            if let Some(id) = SourceId::from_locked(locked) {
                findings.push(Finding {
                    identity: id,
                    entry:    Entry {
                        path: path.to_vec(),
                        name: strip_disambiguator(key).to_owned(),
                        side: Side::Flake,
                        rev:  locked.full_rev().map(str::to_owned).unwrap_or_default(),
                        lm:   locked.last_modified(),
                    },
                });
            }
        }
    }

    fn scan_tack_inputs(
        &self,
        path: &[String],
        findings: &mut Vec<Finding>,
        transitive: &mut Vec<ScanTarget>,
        diagnostics: &mut Vec<ScanDiagnostic>,
    ) {
        let Some(raw) = self.tack_pins.as_deref() else {
            return;
        };
        let doc = match pins::PinsDoc::parse(raw) {
            Ok(doc) => doc,
            Err(err) => {
                diagnostics.push(ScanDiagnostic::parse(path, ScanFile::TackPins, err));
                return;
            },
        };
        let tinputs = match doc.inputs() {
            Ok(inputs) => inputs,
            Err(err) => {
                diagnostics.push(ScanDiagnostic::config(path, ScanFile::TackPins, err));
                return;
            },
        };
        let tlock = self.parse_tack_lock(path, diagnostics);
        let tshort = doc.shorturls();
        for tinp in &tinputs {
            let expanded = tshort.expand(&tinp.url);
            Self::record_tack_finding(path, tinp, &expanded, &tlock, findings);
            Self::queue_tack_transitive(path, tinp, expanded, &tlock, transitive);
        }
    }

    fn parse_tack_lock(
        &self,
        path: &[String],
        diagnostics: &mut Vec<ScanDiagnostic>,
    ) -> lock::LockFile {
        let Some(raw_lock) = self.tack_lock.as_deref() else {
            return lock::LockFile::new();
        };
        match lock::parse(raw_lock) {
            Ok(lock) => lock,
            Err(err) => {
                diagnostics.push(ScanDiagnostic::parse(path, ScanFile::TackLock, err));
                lock::LockFile::new()
            },
        }
    }

    fn record_tack_finding(
        path: &[String],
        input: &pins::Input,
        expanded: &str,
        lock: &lock::LockFile,
        findings: &mut Vec<Finding>,
    ) {
        if let Some(id) = SourceId::from_url(expanded) {
            findings.push(Finding {
                identity: id,
                entry:    Entry {
                    path: path.to_vec(),
                    name: input.name.clone(),
                    side: Side::Tack,
                    rev:  lock
                        .get(&input.name)
                        .and_then(|n| n.full_rev().map(str::to_owned))
                        .unwrap_or_default(),
                    lm:   lock.get(&input.name).and_then(LockedNode::last_modified),
                },
            });
        }
    }

    fn queue_tack_transitive(
        path: &[String],
        input: &pins::Input,
        expanded: String,
        lock: &lock::LockFile,
        transitive: &mut Vec<ScanTarget>,
    ) {
        if input.pin_type == PinType::Fixed {
            return;
        }
        let mut next = path.to_vec();
        next.push(input.name.clone());
        let source = lock
            .get(&input.name)
            .cloned()
            .map_or(SourceRef::Url(expanded), SourceRef::Locked);
        transitive.push(ScanTarget {
            path: next,
            source,
            submodules: input.submodules,
        });
    }
}

/// fetch one file from a locked node via raw http
/// unknown hosts or missing revs tell the caller to skip the raw path
pub(super) fn try_raw_file(
    node: &LockedNode,
    file: ScanFile,
) -> Result<Option<String>, FetchError> {
    let Some(probe) = RawProbe::from_locked(node) else {
        return Ok(None);
    };
    probe.fetch(file).map(Some)
}

/// flake.lock disambiguates same-named nodes as `name_2`, `name_3`
/// recover the original input name so dedup groups by what the parent flake
/// actually declares
pub(super) fn strip_disambiguator(key: &str) -> &str {
    let bytes = key.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i > 0 && i < bytes.len() && bytes[i - 1] == b'_' {
        key.get(..i - 1).unwrap_or(key)
    } else {
        key
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ScanDocuments,
        SourceRef,
    };
    use crate::{
        lock::LockedNode,
        render,
        scan_diagnostic::{
            ScanDiagnostic,
            ScanFile,
        },
    };

    fn github_node(owner: &str, repo: &str, rev: &str) -> LockedNode {
        LockedNode::new_github(owner, repo, rev, "sha256-test", 0)
    }

    fn assert_parse_diagnostic(diagnostic: &ScanDiagnostic, path: &[String], file: ScanFile) {
        assert_eq!(diagnostic.path(), path);
        assert!(
            render::scan_diagnostic(diagnostic)
                .starts_with(&format!("scan {}: {file} parse failed:", path.join(" > ")))
        );
    }

    #[test]
    fn source_ref_key_distinguishes_locked_revisions() {
        let first = SourceRef::Locked(github_node("Owner", "Repo", "rev-a")).key();
        let second = SourceRef::Locked(github_node("owner", "repo", "rev-b")).key();

        assert_ne!(first, second);
    }

    #[test]
    fn source_ref_key_dedupes_same_locked_revision() {
        let first = SourceRef::Locked(github_node("Owner", "Repo", "rev-a")).key();
        let second = SourceRef::Locked(github_node("owner", "repo", "rev-a")).key();

        assert_eq!(first, second);
    }

    #[test]
    fn source_ref_key_preserves_unlocked_url_specificity() {
        let first = SourceRef::Url("github:Owner/Repo?rev=rev-a".to_owned()).key();
        let second = SourceRef::Url("github:Owner/Repo?rev=rev-b".to_owned()).key();

        assert_ne!(first, second);
    }

    #[test]
    fn scan_reports_flake_lock_parse_failure() {
        let path = vec!["root".to_owned()];
        let result = ScanDocuments {
            flake_lock: Some("{".to_owned()),
            tack_pins:  None,
            tack_lock:  None,
        }
        .scan(&path);

        assert!(result.findings.is_empty());
        assert!(result.transitive.is_empty());
        assert_eq!(result.diagnostics.len(), 1);
        assert_parse_diagnostic(&result.diagnostics[0], &path, ScanFile::FlakeLock);
    }

    #[test]
    fn scan_reports_tack_lock_parse_failure_and_continues() {
        let path = vec!["root".to_owned()];
        let result = ScanDocuments {
            flake_lock: None,
            tack_pins:  Some(
                r#"
                [inputs.dep]
                url = "github:Owner/Repo"
                "#
                .to_owned(),
            ),
            tack_lock:  Some("{".to_owned()),
        }
        .scan(&path);

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].identity.to_string(), "github:owner/repo");
        assert_eq!(result.transitive.len(), 1);
        assert_eq!(result.diagnostics.len(), 1);
        assert_parse_diagnostic(&result.diagnostics[0], &path, ScanFile::TackLock);
    }
}
