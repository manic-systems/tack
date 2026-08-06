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

use misstep::Result;

use super::{
    super::tolerate,
    TargetPin,
    model::{
        Entry,
        Identity,
    },
};
use crate::{
    fetch::{
        self,
        FetchError,
    },
    lock::{
        self,
        FlakeInputRef,
        FlakeLock,
        LockIdentity,
        LockedNode,
    },
    pins::{
        self,
        PinType,
        Side,
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

const MAX_WALK_DEPTH: usize = 128;

pub(super) struct Finding {
    pub identity: SourceId,
    pub entry:    Entry,
}

pub(super) struct ScanResult {
    pub findings:    Vec<Finding>,
    pub followed:    Vec<FollowedInput>,
    pub transitive:  Vec<ScanTarget>,
    pub registry:    Vec<(String, Registration)>,
    pub diagnostics: Vec<ScanDiagnostic>,
}

/// a nested doc's pin that its own follows can target
pub(super) enum Registration {
    Pin(Box<TargetPin>),
    /// a pin the consumer already followed elsewhere, named by that target
    Alias(String),
}

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

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct OmitLayer {
    omitted: BTreeSet<String>,
    kept:    BTreeSet<String>,
}

impl OmitLayer {
    fn new(
        global: &BTreeSet<String>,
        local_omit: &BTreeSet<String>,
        local_keep: &BTreeSet<String>,
    ) -> Self {
        Self {
            omitted: global.union(local_omit).cloned().collect(),
            kept:    local_keep.clone(),
        }
    }

    fn is_empty(&self) -> bool {
        self.omitted.is_empty() && self.kept.is_empty()
    }
}

/// layers ordered outermost first, so a parent's rules outrank a child's
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct OmitPolicy {
    layers: Vec<OmitLayer>,
}

impl OmitPolicy {
    pub(super) fn for_input(global: &BTreeSet<String>, input: &pins::Input) -> Self {
        Self::default().descend(global, &input.omit_inputs, &input.keep_inputs)
    }

    /// a keep cancels omits at its own layer and deeper, never outer ones
    pub(super) fn omits(&self, side: Side, name: &str) -> bool {
        for layer in &self.layers {
            if pins::rules_match(&layer.kept, side, name) {
                return false;
            }
            if pins::rules_match(&layer.omitted, side, name) {
                return true;
            }
        }
        false
    }

    pub(super) fn synthetic(global: &BTreeSet<String>) -> Self {
        Self::default().descend(global, &BTreeSet::new(), &BTreeSet::new())
    }

    /// cross a tack boundary: the nested doc's global table and the
    /// descended-into pin's own rules form one new innermost layer
    fn descend(
        &self,
        doc_omit: &BTreeSet<String>,
        local_omit: &BTreeSet<String>,
        local_keep: &BTreeSet<String>,
    ) -> Self {
        let mut layers = self.layers.clone();
        let layer = OmitLayer::new(doc_omit, local_omit, local_keep);
        if !layer.is_empty() {
            layers.push(layer);
        }
        Self { layers }
    }
}

#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct FollowPolicy {
    deep:      BTreeMap<String, String>,
    inherited: BTreeMap<String, String>,
    level:     BTreeMap<String, String>,
    excludes:  BTreeSet<String>,
    /// `pin/` for the pin these rules apply to, whose first level skips deep
    /// rules into its own inputs as the resolver does
    own:       String,
}

impl FollowPolicy {
    pub(super) fn for_input(all_follow: &BTreeMap<String, String>, input: &pins::Input) -> Self {
        Self {
            deep:      all_follow.clone(),
            inherited: BTreeMap::new(),
            level:     input.follows.clone(),
            excludes:  input.excludes.clone(),
            own:       format!("{}/", input.name),
        }
    }

    fn target<'a>(&'a self, side: Side, name: &str, at_level: bool) -> Option<&'a str> {
        let scoped = format!("{side}:{name}");
        if let Some(target) = self.inherited.get(&scoped) {
            return Some(target);
        }
        if at_level && let Some(target) = self.level.get(name).or_else(|| self.level.get(&scoped)) {
            return Some(target);
        }
        self.deep_target(side, name)
            .filter(|target| !(at_level && self.is_own(target)))
    }

    /// whether other pins follow this level's `name`, which then can't be
    /// omitted
    fn followed_into(&self, side: Side, name: &str, at_level: bool) -> bool {
        at_level
            && self
                .deep_rule(side, name)
                .is_some_and(|target| self.is_own(target))
    }

    fn is_own(&self, target: &str) -> bool {
        !self.own.is_empty() && target.starts_with(&self.own)
    }

    fn deep_target(&self, side: Side, name: &str) -> Option<&str> {
        if pins::rules_match(&self.excludes, side, name) {
            return None;
        }
        self.deep_rule(side, name)
    }

    /// excludes only stop this pin following a rule, other pins still follow
    /// into it
    fn deep_rule(&self, side: Side, name: &str) -> Option<&str> {
        self.deep
            .get(name)
            .or_else(|| self.deep.get(&format!("{side}:{name}")))
            .map(String::as_str)
    }

    pub(super) fn synthetic(all_follow: &BTreeMap<String, String>, name: &str) -> Self {
        Self {
            deep:      all_follow.clone(),
            inherited: BTreeMap::new(),
            level:     BTreeMap::new(),
            excludes:  BTreeSet::new(),
            own:       format!("{name}/"),
        }
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
        pin: &str,
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
            own:       format!("{namespace}::{pin}/"),
        }
    }

    /// project deep rules per side with exclusions applied, inherited rules
    /// layered over them; mirrors the resolver's propagated `__tack_policy`
    fn effective_scoped(&self) -> BTreeMap<String, String> {
        let mut scoped = BTreeMap::new();
        for side in [Side::Flake, Side::Tack] {
            for (key, target) in &self.deep {
                let Some(name) = pins::rule_name(key, side) else {
                    continue;
                };
                if !pins::rules_match(&self.excludes, side, name) {
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
    path:       &'a [String],
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
        None if !follow.followed_into(side, name, at_level) && omit.omits(side, name) => {
            InputDecision::Omit
        },
        None => InputDecision::Traverse,
    }
}

pub(super) enum FlakeVisit<'a> {
    Followed {
        path:   &'a [String],
        name:   &'a str,
        target: &'a str,
    },
    Locked {
        path:   &'a [String],
        name:   &'a str,
        locked: &'a LockedNode,
    },
    Unresolved(String),
    TooDeep,
}

/// each node's inputs are expanded once, so shared subgraphs cost one visit
/// while every incoming edge is still reported at its first-seen path
pub(super) fn walk_flake_lock<'a>(
    doc: &'a FlakeLock,
    root: &'a lock::FlakeNode,
    path: &[String],
    omit: &OmitPolicy,
    follow: &'a FollowPolicy,
    mut visit: impl FnMut(FlakeVisit<'_>),
) {
    let mut expanded = BTreeSet::from([doc.root()]);
    let mut stack = vec![(path.to_vec(), root.inputs())];
    let mut truncated = false;
    loop {
        let depth = stack.len();
        let Some(&mut (ref at, ref mut edges)) = stack.last_mut() else {
            break;
        };
        let Some((name, input_ref)) = edges.next() else {
            stack.pop();
            continue;
        };
        match decide_input(omit, follow, Side::Flake, name, depth == 1) {
            InputDecision::Follow(target) => {
                visit(FlakeVisit::Followed {
                    path: at,
                    name,
                    target,
                });
            },
            InputDecision::Omit => {},
            InputDecision::Traverse => {
                // a follows path aliases the edge it names, which the walk counts there
                let FlakeInputRef::Node(ref target) = *input_ref else {
                    continue;
                };
                let Some((node_name, child)) = doc.node(target) else {
                    visit(FlakeVisit::Unresolved(format!(
                        "node '{target}' does not exist"
                    )));
                    continue;
                };
                if let Some(locked) = child.locked() {
                    visit(FlakeVisit::Locked {
                        path: at,
                        name,
                        locked,
                    });
                }
                if depth >= MAX_WALK_DEPTH {
                    truncated = true;
                } else if expanded.insert(node_name) {
                    let next = child_path(at, name);
                    stack.push((next, child.inputs()));
                }
            },
        }
    }
    if truncated {
        visit(FlakeVisit::TooDeep);
    }
}

fn child_path(path: &[String], name: &str) -> Vec<String> {
    let mut child = path.to_vec();
    child.push(name.to_owned());
    child
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
        let mut probe_diagnostics = Vec::new();
        if let SourceRef::Locked(ref node) = self.source {
            let probe = RawProbe::documents(node, &self.path);
            probe_diagnostics.extend(probe.diagnostics);
            if let Some(documents) = probe.documents.or_else(|| ScanDocuments::sparse(node)) {
                return Ok(LoadedDocuments {
                    documents,
                    diagnostics: probe_diagnostics,
                });
            }
            if let Some(private_repo) = probe.private_repo {
                probe_diagnostics.push(private_repo);
                return Ok(LoadedDocuments {
                    documents:   ScanDocuments::empty(),
                    diagnostics: probe_diagnostics,
                });
            }
        }

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
    documents:    Option<ScanDocuments>,
    diagnostics:  BTreeSet<ScanDiagnostic>,
    private_repo: Option<ScanDiagnostic>,
}

impl RawProbeOutcome {
    const fn empty() -> Self {
        Self {
            documents:    None,
            diagnostics:  BTreeSet::new(),
            private_repo: None,
        }
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
                private_repo: None,
            }
        } else if all_missing && fetch::forge_miss_untrusted(&self.forge) {
            RawProbeOutcome {
                documents: None,
                diagnostics,
                private_repo: Some(ScanDiagnostic::private_repo(
                    path,
                    ScanFile::FlakeLock,
                    self.forge.base(),
                )),
            }
        } else {
            RawProbeOutcome {
                documents: Some(documents),
                diagnostics,
                private_repo: None,
            }
        }
    }

    fn fetch(&self, file: ScanFile) -> Result<String, FetchError> {
        fetch::forge_raw_file(&self.forge, self.rev, file.as_path())
    }
}

impl ScanDocuments {
    const fn empty() -> Self {
        Self {
            flake_lock: None,
            tack_pins:  None,
            tack_lock:  None,
        }
    }

    fn sparse(node: &LockedNode) -> Option<Self> {
        let files = [ScanFile::FlakeLock, ScanFile::TackPins, ScanFile::TackLock];
        let paths = files.map(ScanFile::as_path);
        let mut contents = fetch::fetch_locked_scan_files(node, &paths)
            .ok()??
            .into_iter();
        Some(Self {
            flake_lock: contents.next()?,
            tack_pins:  contents.next()?,
            tack_lock:  contents.next()?,
        })
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
        let mut registry = Vec::<(String, Registration)>::new();
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
        let Some(root) = doc.root_node() else {
            diagnostics.push(ScanDiagnostic::parse(
                path,
                ScanFile::FlakeLock,
                format!("flake lock root node '{}' does not exist", doc.root()),
            ));
            return;
        };
        walk_flake_lock(&doc, root, path, omit, follow, |visit| {
            match visit {
                FlakeVisit::Followed {
                    path: at,
                    name,
                    target,
                } => {
                    followed.push(FollowedInput {
                        target: target.to_owned(),
                        path:   at.to_vec(),
                        name:   name.to_owned(),
                        side:   Side::Flake,
                    });
                },
                FlakeVisit::Locked {
                    path: at,
                    name,
                    locked,
                } => Self::record_flake_finding(at, name, locked, findings),
                FlakeVisit::Unresolved(err) => {
                    diagnostics.push(ScanDiagnostic::parse(path, ScanFile::FlakeLock, err));
                },
                FlakeVisit::TooDeep => {
                    diagnostics.push(ScanDiagnostic::truncated(
                        path,
                        ScanFile::FlakeLock,
                        format!(
                            "inputs nest deeper than {MAX_WALK_DEPTH}, deeper ones were skipped"
                        ),
                    ));
                },
            }
        });
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
                    path:     path.to_vec(),
                    name:     name.to_owned(),
                    side:     Side::Flake,
                    identity: locked.source_identity().map(Identity::from_lock),
                    lm:       locked.last_modified(),
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
        findings: &mut Vec<Finding>,
        followed: &mut Vec<FollowedInput>,
        transitive: &mut Vec<ScanTarget>,
        registry: &mut Vec<(String, Registration)>,
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
        // policy only crosses into a pin wired for recomposition, mirroring the
        // resolver's gate
        let inert = (OmitPolicy::default(), FollowPolicy::default());
        let Some((recomposable, doc_omit, doc_follow)) = Self::doc_tables(&doc, path, diagnostics)
        else {
            return;
        };
        let (omit, follow) = if recomposable {
            (arrived_omit, arrived_follow)
        } else {
            (&inert.0, &inert.1)
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
        let namespace = path.join("/");
        let context = DocContext {
            path,
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
                    let expanded = match tshort.expand(&tinp.url) {
                        Ok(expanded) => expanded,
                        Err(err) => {
                            diagnostics.push(ScanDiagnostic::config(path, ScanFile::TackPins, err));
                            continue;
                        },
                    };
                    let child_omit = omit.descend(&doc_omit, &tinp.omit_inputs, &tinp.keep_inputs);
                    let child_follow = follow.descend(
                        &doc_follow,
                        &tinp.follows,
                        &tinp.excludes,
                        &namespace,
                        &tinp.name,
                    );
                    Self::record_tack_finding(path, tinp, &expanded, &tlock, findings);
                    if referenced.contains(&tinp.name) {
                        registry.push((
                            format!("{namespace}::{}", tinp.name),
                            Registration::Pin(Box::new(TargetPin::declared(
                                tinp,
                                &expanded,
                                tlock.get(&tinp.name),
                                child_path(path, &tinp.name),
                                child_omit.clone(),
                                child_follow.clone(),
                            ))),
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
        Self::register_doc_targets(&tinputs, &tlock, &context, &referenced, followed, registry);
    }

    fn doc_tables(
        doc: &pins::PinsDoc,
        path: &[String],
        diagnostics: &mut Vec<ScanDiagnostic>,
    ) -> Option<(bool, BTreeSet<String>, BTreeMap<String, String>)> {
        let recomposable = match doc.is_recomposable() {
            Ok(recomposable) => recomposable,
            Err(err) => {
                diagnostics.push(ScanDiagnostic::config(path, ScanFile::TackPins, err));
                return None;
            },
        };
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
        Some((recomposable, doc_omit, doc_follow))
    }

    /// the pins this doc's follows can name, including lock-only ones the
    /// resolver's autoPin synthesises
    fn register_doc_targets(
        tinputs: &[pins::Input],
        tlock: &lock::LockFile,
        context: &DocContext<'_>,
        referenced: &BTreeSet<String>,
        followed: &[FollowedInput],
        registry: &mut Vec<(String, Registration)>,
    ) {
        for entry in followed.iter().filter(|entry| entry.side == Side::Tack) {
            if referenced.contains(&entry.name) {
                let alias = Registration::Alias(entry.target.clone());
                registry.push((format!("{}::{}", context.namespace, entry.name), alias));
            }
        }
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
            registry.push((
                format!("{}::{name}", context.namespace),
                Registration::Pin(Box::new(TargetPin {
                    path:          child_path(context.path, name),
                    identity:      SourceId::from_locked(node),
                    lock_identity: node.source_identity().map(Identity::from_lock),
                    lm:            node.last_modified(),
                    source:        Some(SourceRef::Locked(node.clone())),
                    submodules:    false,
                    pin_type:      PinType::Flake,
                    omit:          context.omit.descend(
                        context.doc_omit,
                        &BTreeSet::new(),
                        &BTreeSet::new(),
                    ),
                    follow:        context.follow.descend(
                        context.doc_follow,
                        &BTreeMap::new(),
                        &BTreeSet::new(),
                        context.namespace,
                        name,
                    ),
                })),
            ));
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
                    path:     path.to_vec(),
                    name:     input.name.clone(),
                    side:     Side::Tack,
                    identity: node
                        .and_then(LockedNode::source_identity)
                        .map(Identity::from_lock),
                    lm:       node.and_then(LockedNode::last_modified),
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
        let locked = lock.get(&input.name).cloned();
        if input.tag.is_some() && locked.is_none() {
            return;
        }
        let next = child_path(path, &input.name);
        let source = locked.map_or(SourceRef::Url(expanded), SourceRef::Locked);
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

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
