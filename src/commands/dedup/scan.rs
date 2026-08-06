// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    fs,
    path::{
        Path,
        PathBuf,
    },
};

use eyre::Result;

use super::{
    super::tolerate,
    TargetPin,
    model::{
        Entry,
        Side,
    },
};
use crate::{
    fetch::{
        self,
        FetchError,
    },
    lock::{
        self,
        FlakeLock,
        LockIdentity,
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
    pub followed:    Vec<FollowedInput>,
    pub transitive:  Vec<ScanTarget>,
    pub registry:    Vec<(String, TargetPin)>,
    pub diagnostics: Vec<ScanDiagnostic>,
}

#[derive(PartialEq, Eq)]
pub(super) struct FollowedInput {
    pub target: String,
    pub path:   Vec<String>,
    pub name:   String,
    pub side:   Side,
}

#[derive(Clone)]
pub(super) struct ScanTarget {
    pub path:       Vec<String>,
    pub source:     SourceRef,
    pub submodules: bool,
    /// how the pin is consumed: a fetch drill-in always receives policy, a
    /// flake pin only under the publishing contract (recomposable upstream)
    pub pin_type:   PinType,
    pub omit:       OmitPolicy,
    pub follow:     FollowPolicy,
    pub ancestors:  BTreeSet<String>,
}

#[derive(Clone, Default)]
pub(super) struct OmitPolicy {
    omitted: BTreeSet<String>,
    kept:    BTreeSet<String>,
}

impl OmitPolicy {
    pub(super) fn for_input(global: &BTreeSet<String>, input: &pins::Input) -> Self {
        let omitted = global.union(&input.omit_inputs).cloned().collect();
        let kept = input.keep_inputs.clone();
        Self { omitted, kept }
    }

    pub(super) fn omits(&self, side: Side, name: &str) -> bool {
        matches_rule(&self.omitted, side, name) && !matches_rule(&self.kept, side, name)
    }

    pub(super) fn synthetic(global: &BTreeSet<String>) -> Self {
        Self {
            omitted: global.clone(),
            kept:    BTreeSet::new(),
        }
    }

    fn is_active(&self) -> bool {
        !self.omitted.is_empty() || !self.kept.is_empty()
    }

    /// cross a tack boundary: inherited rules plus the nested doc's global
    /// table plus the descended-into pin's own rules
    fn descend(
        &self,
        doc_omit: &BTreeSet<String>,
        local_omit: &BTreeSet<String>,
        local_keep: &BTreeSet<String>,
    ) -> Self {
        Self {
            omitted: self
                .omitted
                .iter()
                .chain(doc_omit)
                .chain(local_omit)
                .cloned()
                .collect(),
            kept:    self.kept.iter().chain(local_keep).cloned().collect(),
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct FollowPolicy {
    deep:      BTreeMap<String, String>,
    inherited: BTreeMap<String, String>,
    level:     BTreeMap<String, String>,
    excludes:  BTreeSet<String>,
}

impl FollowPolicy {
    pub(super) fn for_input(all_follow: &BTreeMap<String, String>, input: &pins::Input) -> Self {
        Self {
            deep:      all_follow.clone(),
            inherited: BTreeMap::new(),
            level:     input.follows.clone(),
            excludes:  input.excludes.clone(),
        }
    }

    fn target<'a>(&'a self, side: Side, name: &str, at_level: bool) -> Option<&'a str> {
        let follow_side = match side {
            Side::Flake => pins::FollowSide::Flake,
            Side::Tack => pins::FollowSide::Tack,
        };
        let scoped = format!("{side}:{name}");
        if at_level && let Some(target) = self.level.get(name).or_else(|| self.level.get(&scoped)) {
            return Some(target);
        }
        if let Some(target) = self.inherited.get(&scoped) {
            return Some(target);
        }
        pins::global_follow_target(&self.deep, &self.excludes, follow_side, name)
    }

    pub(super) fn synthetic(all_follow: &BTreeMap<String, String>) -> Self {
        Self {
            deep:      all_follow.clone(),
            inherited: BTreeMap::new(),
            level:     BTreeMap::new(),
            excludes:  BTreeSet::new(),
        }
    }

    fn is_active(&self) -> bool {
        !self.deep.is_empty() || !self.inherited.is_empty() || !self.level.is_empty()
    }

    /// cross a tack boundary: surviving rules become inherited (immune to the
    /// nested doc's exclusions), the nested doc's tables become the new global
    /// and level layers, rekeyed into its namespace
    fn descend(
        &self,
        doc_follow: &BTreeMap<String, String>,
        level: &BTreeMap<String, String>,
        excludes: &BTreeSet<String>,
        namespace: &str,
    ) -> Self {
        let qualify = |target: &String| format!("{namespace}::{target}");
        Self {
            deep:      doc_follow
                .iter()
                .map(|(name, target)| (name.clone(), qualify(target)))
                .collect(),
            inherited: self.effective_scoped(),
            level:     level
                .iter()
                .map(|(name, target)| (name.clone(), qualify(target)))
                .collect(),
            excludes:  excludes.clone(),
        }
    }

    /// project deep rules per side with exclusions applied, inherited rules
    /// layered over them; mirrors the resolver's propagated `__tack_policy`
    fn effective_scoped(&self) -> BTreeMap<String, String> {
        let mut scoped = BTreeMap::new();
        for side in [pins::FollowSide::Flake, pins::FollowSide::Tack] {
            for (key, target) in &self.deep {
                let Some(name) = pins::FollowAlias::from(key.as_str()).on_side(side) else {
                    continue;
                };
                if !pins::is_global_follow_excluded(&self.excludes, side, name) {
                    scoped.insert(format!("{side}:{name}"), target.clone());
                }
            }
        }
        scoped.extend(
            self.inherited
                .iter()
                .map(|(name, target)| (name.clone(), target.clone())),
        );
        scoped
    }
}

/// one scanned doc's policy vantage: the traversal policies that arrived here
/// plus the doc's own global tables, namespaced to its pins
struct DocContext<'a> {
    namespace:  &'a str,
    omit:       &'a OmitPolicy,
    follow:     &'a FollowPolicy,
    doc_omit:   &'a BTreeSet<String>,
    doc_follow: &'a BTreeMap<String, String>,
}

enum InputDecision<'a> {
    Follow(&'a str),
    Omit,
    Traverse,
}

fn decide_input<'a>(
    omit: &OmitPolicy,
    follow: &'a FollowPolicy,
    side: Side,
    name: &str,
    at_level: bool,
) -> InputDecision<'a> {
    match follow.target(side, name, at_level) {
        Some(target) => InputDecision::Follow(target),
        None if omit.omits(side, name) => InputDecision::Omit,
        None => InputDecision::Traverse,
    }
}

fn matches_rule(rules: &BTreeSet<String>, side: Side, name: &str) -> bool {
    rules.iter().any(|rule| {
        match rule.split_once(':') {
            Some(("flake", input)) => side == Side::Flake && (input == "*" || input == name),
            Some(("tack", input)) => side == Side::Tack && (input == "*" || input == name),
            _ => rule == "*" || rule == name,
        }
    })
}

impl ScanTarget {
    pub(super) fn key(&self) -> String {
        let source = self.source.key();
        SourceRef::tagged_key("scan", &[
            source.as_str(),
            if self.submodules { "1" } else { "0" },
        ])
    }

    pub(super) fn load_documents(&self) -> Result<LoadedDocuments> {
        let probe_diagnostics = if let SourceRef::Locked(ref node) = self.source {
            let (maybe_documents, diagnostics) = RawProbe::documents(node, &self.path).into_parts();
            if let Some(documents) = maybe_documents {
                return Ok(LoadedDocuments {
                    documents,
                    diagnostics,
                });
            }
            diagnostics
        } else {
            Vec::new()
        };

        let tmp = tempfile::tempdir()?;
        let root = self.fetch_tree(tmp.path())?;
        Ok(LoadedDocuments {
            documents:   ScanDocuments::from_tree(&root),
            diagnostics: probe_diagnostics,
        })
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

#[derive(Clone)]
pub(super) enum SourceRef {
    Locked(LockedNode),
    Url(String),
}

impl SourceRef {
    pub(super) fn key(&self) -> String {
        match *self {
            Self::Locked(ref node) => {
                // transitive deps can differ across revisions
                let rev = node
                    .source_identity()
                    .map(LockIdentity::into_string)
                    .unwrap_or_default();
                SourceId::from_locked(node).map_or_else(
                    || Self::tagged_key("locked", &[node.kind(), rev.as_str()]),
                    |source_id| {
                        let identity = source_id.to_string();
                        Self::tagged_key("locked", &[&identity, rev.as_str()])
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

#[derive(Clone)]
pub(super) struct ScanDocuments {
    flake_lock: Option<String>,
    tack_pins:  Option<String>,
    tack_lock:  Option<String>,
}

pub(super) struct LoadedDocuments {
    pub documents:   ScanDocuments,
    pub diagnostics: Vec<ScanDiagnostic>,
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

    fn into_parts(self) -> (Option<ScanDocuments>, Vec<ScanDiagnostic>) {
        (self.documents, self.diagnostics.into_iter().collect())
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
            rev:   node.forge_rev()?,
        })
    }

    /// non-authoritative misses fall back to clone scanning
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
        let all_missing = documents.flake_lock.is_none()
            && documents.tack_pins.is_none()
            && documents.tack_lock.is_none();
        if (!self.forge.authoritative() || !diagnostics.is_empty()) && all_missing {
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
    #[cfg(test)]
    pub(super) fn from_raw(
        flake_lock: Option<&str>,
        tack_pins: Option<&str>,
        tack_lock: Option<&str>,
    ) -> Self {
        Self {
            flake_lock: flake_lock.map(str::to_owned),
            tack_pins:  tack_pins.map(str::to_owned),
            tack_lock:  tack_lock.map(str::to_owned),
        }
    }

    fn from_tree(root: &Path) -> Self {
        let flake_lock = fs::read_to_string(root.join("flake.lock")).ok();
        let td = root.join(".tack");
        Self {
            flake_lock,
            tack_pins: fs::read_to_string(td.join("pins.toml")).ok(),
            tack_lock: fs::read_to_string(td.join("pins.lock.json")).ok(),
        }
    }

    pub(super) fn scan(
        &self,
        path: &[String],
        omit: &OmitPolicy,
        follow: &FollowPolicy,
        pin_type: PinType,
    ) -> ScanResult {
        let mut findings = Vec::<Finding>::new();
        let mut followed = Vec::<FollowedInput>::new();
        let mut transitive = Vec::<ScanTarget>::new();
        let mut registry = Vec::<(String, TargetPin)>::new();
        let mut diagnostics = Vec::<ScanDiagnostic>::new();

        // a fetch pin is a bare source tree: only its tack side is live
        if pin_type != PinType::Fetch {
            self.scan_flake_lock(
                path,
                omit,
                follow,
                &mut findings,
                &mut followed,
                &mut diagnostics,
            );
        }
        self.scan_tack_inputs(
            path,
            omit,
            follow,
            pin_type,
            &mut findings,
            &mut followed,
            &mut transitive,
            &mut registry,
            &mut diagnostics,
        );

        ScanResult {
            findings,
            followed,
            transitive,
            registry,
            diagnostics,
        }
    }

    fn scan_flake_lock(
        &self,
        path: &[String],
        omit: &OmitPolicy,
        follow: &FollowPolicy,
        findings: &mut Vec<Finding>,
        followed: &mut Vec<FollowedInput>,
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
        let Some(root) = doc.root() else {
            diagnostics.push(ScanDiagnostic::parse(
                path,
                ScanFile::FlakeLock,
                format!("flake lock root node '{}' does not exist", doc.root_name()),
            ));
            return;
        };
        let mut ancestry = BTreeSet::from([doc.root_name().to_owned()]);
        Self::walk_flake_inputs(
            &doc,
            root,
            path,
            omit,
            follow,
            true,
            &mut ancestry,
            findings,
            followed,
            diagnostics,
        );
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "graph traversal state is explicit"
    )]
    fn walk_flake_inputs(
        doc: &FlakeLock,
        node: &lock::FlakeNode,
        path: &[String],
        omit: &OmitPolicy,
        follow: &FollowPolicy,
        at_level: bool,
        ancestry: &mut BTreeSet<String>,
        findings: &mut Vec<Finding>,
        followed: &mut Vec<FollowedInput>,
        diagnostics: &mut Vec<ScanDiagnostic>,
    ) {
        for (name, input_ref) in node.inputs() {
            match decide_input(omit, follow, Side::Flake, name, at_level) {
                InputDecision::Follow(target) => {
                    followed.push(FollowedInput {
                        target: target.to_owned(),
                        path:   path.to_vec(),
                        name:   name.to_owned(),
                        side:   Side::Flake,
                    });
                },
                InputDecision::Omit => {},
                InputDecision::Traverse => {
                    let (node_name, child) = match doc.resolve_input_ref(input_ref) {
                        Ok(resolved) => resolved,
                        Err(err) => {
                            diagnostics.push(ScanDiagnostic::parse(path, ScanFile::FlakeLock, err));
                            continue;
                        },
                    };
                    if let Some(locked) = child.locked() {
                        Self::record_flake_finding(path, name, locked, findings);
                    }
                    if ancestry.insert(node_name.to_owned()) {
                        let mut child_path = path.to_vec();
                        child_path.push(name.to_owned());
                        Self::walk_flake_inputs(
                            doc,
                            child,
                            &child_path,
                            omit,
                            follow,
                            false,
                            ancestry,
                            findings,
                            followed,
                            diagnostics,
                        );
                        ancestry.remove(node_name);
                    }
                },
            }
        }
    }

    fn record_flake_finding(
        path: &[String],
        name: &str,
        locked: &LockedNode,
        findings: &mut Vec<Finding>,
    ) {
        if let Some(identity) = SourceId::from_locked(locked) {
            findings.push(Finding {
                identity,
                entry: Entry {
                    path: path.to_vec(),
                    name: name.to_owned(),
                    side: Side::Flake,
                    rev:  locked
                        .source_identity()
                        .map(LockIdentity::into_string)
                        .unwrap_or_default(),
                    lm:   locked.last_modified(),
                },
            });
        }
    }

    #[expect(clippy::too_many_arguments, reason = "scan accumulators are explicit")]
    fn scan_tack_inputs(
        &self,
        path: &[String],
        arrived_omit: &OmitPolicy,
        arrived_follow: &FollowPolicy,
        pin_type: PinType,
        findings: &mut Vec<Finding>,
        followed: &mut Vec<FollowedInput>,
        transitive: &mut Vec<ScanTarget>,
        registry: &mut Vec<(String, TargetPin)>,
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
        // policy reaches a fetch drill-in directly; a flake pin only forwards
        // it when wired for recomposition, mirroring the resolver's gate
        let inert = (OmitPolicy::default(), FollowPolicy::default());
        let (omit, follow) = if pin_type == PinType::Fetch || doc.is_recomposable() {
            (arrived_omit, arrived_follow)
        } else {
            if arrived_omit.is_active() || arrived_follow.is_active() {
                diagnostics.push(ScanDiagnostic::config(
                    path,
                    ScanFile::TackPins,
                    "not marked recomposable; inherited follow/omit rules do not reach its tack \
                     pins",
                ));
            }
            (&inert.0, &inert.1)
        };
        let tinputs = match doc.inputs() {
            Ok(inputs) => inputs,
            Err(err) => {
                diagnostics.push(ScanDiagnostic::config(path, ScanFile::TackPins, err));
                return;
            },
        };
        let Some((doc_omit, doc_follow)) = Self::doc_tables(&doc, path, diagnostics) else {
            return;
        };
        let tlock = self.parse_tack_lock(path, diagnostics);
        let tshort = doc.shorturls();
        let namespace = path.join("/");
        let context = DocContext {
            namespace: &namespace,
            omit,
            follow,
            doc_omit: &doc_omit,
            doc_follow: &doc_follow,
        };
        // this doc's follow rules resolve in its own pin namespace
        let referenced = doc_follow
            .values()
            .chain(tinputs.iter().flat_map(|tinp| tinp.follows.values()))
            .cloned()
            .collect::<BTreeSet<String>>();
        for tinp in &tinputs {
            match decide_input(omit, follow, Side::Tack, &tinp.name, true) {
                InputDecision::Follow(target) => {
                    followed.push(FollowedInput {
                        target: target.to_owned(),
                        path:   path.to_vec(),
                        name:   tinp.name.clone(),
                        side:   Side::Tack,
                    });
                },
                InputDecision::Omit => {},
                InputDecision::Traverse => {
                    let expanded = tshort.expand(&tinp.url);
                    let child_omit = omit.descend(&doc_omit, &tinp.omit_inputs, &tinp.keep_inputs);
                    let child_follow =
                        follow.descend(&doc_follow, &tinp.follows, &tinp.excludes, &namespace);
                    Self::record_tack_finding(path, tinp, &expanded, &tlock, findings);
                    if referenced.contains(&tinp.name) {
                        registry.push((
                            format!("{namespace}::{}", tinp.name),
                            Self::registered_target(
                                tinp,
                                &expanded,
                                &tlock,
                                child_omit.clone(),
                                child_follow.clone(),
                            ),
                        ));
                    }
                    Self::queue_tack_transitive(
                        path,
                        tinp,
                        expanded,
                        &tlock,
                        child_omit,
                        child_follow,
                        transitive,
                    );
                },
            }
        }
        Self::register_lock_only_targets(&tinputs, &tlock, &context, &referenced, registry);
    }

    fn doc_tables(
        doc: &pins::PinsDoc,
        path: &[String],
        diagnostics: &mut Vec<ScanDiagnostic>,
    ) -> Option<(BTreeSet<String>, BTreeMap<String, String>)> {
        let doc_omit = match doc.omit_inputs() {
            Ok(names) => names,
            Err(err) => {
                diagnostics.push(ScanDiagnostic::config(path, ScanFile::TackPins, err));
                return None;
            },
        };
        let doc_follow = match doc.all_follows() {
            Ok(aliases) => aliases,
            Err(err) => {
                diagnostics.push(ScanDiagnostic::config(path, ScanFile::TackPins, err));
                return None;
            },
        };
        Some((doc_omit, doc_follow))
    }

    /// lock-only follow targets synthesise into scannable toplevels,
    /// mirroring the resolver's autoPin
    fn register_lock_only_targets(
        tinputs: &[pins::Input],
        tlock: &lock::LockFile,
        context: &DocContext<'_>,
        referenced: &BTreeSet<String>,
        registry: &mut Vec<(String, TargetPin)>,
    ) {
        let declared = tinputs
            .iter()
            .map(|tinp| tinp.name.as_str())
            .collect::<BTreeSet<&str>>();
        for name in referenced {
            if declared.contains(name.as_str()) {
                continue;
            }
            let Some(node) = tlock.get(name) else {
                continue;
            };
            registry.push((format!("{}::{name}", context.namespace), TargetPin {
                identity:   SourceId::from_locked(node),
                rev:        node
                    .source_identity()
                    .map(LockIdentity::into_string)
                    .unwrap_or_default(),
                lm:         node.last_modified(),
                source:     SourceRef::Locked(node.clone()),
                submodules: false,
                pin_type:   PinType::Flake,
                omit:       context.omit.descend(
                    context.doc_omit,
                    &BTreeSet::new(),
                    &BTreeSet::new(),
                ),
                follow:     context.follow.descend(
                    context.doc_follow,
                    &BTreeMap::new(),
                    &BTreeSet::new(),
                    context.namespace,
                ),
            }));
        }
    }

    fn registered_target(
        input: &pins::Input,
        expanded: &str,
        lock: &lock::LockFile,
        omit: OmitPolicy,
        follow: FollowPolicy,
    ) -> TargetPin {
        let node = lock.get(&input.name);
        TargetPin {
            identity: node
                .and_then(SourceId::from_locked)
                .or_else(|| SourceId::from_url(expanded)),
            rev: node
                .and_then(|locked| locked.source_identity().map(LockIdentity::into_string))
                .unwrap_or_default(),
            lm: node.and_then(LockedNode::last_modified),
            source: node
                .cloned()
                .map_or_else(|| SourceRef::Url(expanded.to_owned()), SourceRef::Locked),
            submodules: input.submodules,
            pin_type: input.pin_type,
            omit,
            follow,
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
        match lock::LockFile::parse(raw_lock) {
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
            let node = lock.get(&input.name);
            findings.push(Finding {
                identity: id,
                entry:    Entry {
                    path: path.to_vec(),
                    name: input.name.clone(),
                    side: Side::Tack,
                    rev:  node
                        .and_then(|n| n.source_identity().map(LockIdentity::into_string))
                        .unwrap_or_default(),
                    lm:   node.and_then(LockedNode::last_modified),
                },
            });
        }
    }

    fn queue_tack_transitive(
        path: &[String],
        input: &pins::Input,
        expanded: String,
        lock: &lock::LockFile,
        omit: OmitPolicy,
        follow: FollowPolicy,
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
            pin_type: input.pin_type,
            omit,
            follow,
            ancestors: BTreeSet::new(),
        });
    }
}

pub(super) fn try_raw_file(
    node: &LockedNode,
    file: ScanFile,
) -> Result<Option<String>, FetchError> {
    let Some(probe) = RawProbe::from_locked(node) else {
        return Ok(None);
    };
    probe.fetch(file).map(Some)
}

/// flake.lock disambiguates same-named nodes as `name_2`
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
#[path = "scan_tests.rs"]
mod tests;
