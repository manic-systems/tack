// SPDX-License-Identifier: EUPL-1.2

mod auto;
mod compare;
mod model;
mod reporting;
mod scan;

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    mem,
};

use misstep::Result;

pub(super) use self::auto::{
    AutoDedupReport,
    auto_dedup_scoped,
};
use self::{
    compare::{
        AheadBehindResult,
        ahead_behind,
    },
    model::{
        Entry,
        Identity,
    },
    reporting::build_report,
    scan::{
        FollowPolicy,
        FollowedInput,
        OmitPolicy,
        Registration,
        ScanDocuments,
        ScanTarget,
        SourceRef,
    },
};
use crate::{
    dispatcher,
    lock::{
        LockFile,
        LockedNode,
    },
    pins::{
        self,
        PinType,
        Side,
    },
    project::Project,
    render,
    report::DedupReport,
    scan_diagnostic::{
        ScanDiagnostic,
        ScanFile,
    },
    shorturl::ShortUrls,
    source::id::SourceId,
};

const DEDUP_SCAN_IN_FLIGHT: usize = 16;
const MAX_ROUTE_DEPTH: usize = 64;
/// each route rescans its documents, and per-pin policies multiply routes
/// through a hostile project
const MAX_ROUTES: usize = 4096;

fn top_map<T>(
    inputs: &[pins::Input],
    lock: &LockFile,
    project: impl Fn(&LockedNode) -> Option<T>,
) -> BTreeMap<String, T> {
    let declared = inputs
        .iter()
        .map(|inp| inp.name.as_str())
        .collect::<BTreeSet<&str>>();
    inputs
        .iter()
        .filter_map(|inp| {
            lock.get(&inp.name)
                .and_then(&project)
                .map(|val| (inp.name.clone(), val))
        })
        .chain(lock.iter().filter_map(|(key, node)| {
            (!declared.contains(key.as_str()))
                .then(|| project(node).map(|val| (key.clone(), val)))
                .flatten()
        }))
        .collect()
}

#[derive(Clone)]
struct TargetPin {
    path:          Vec<String>,
    identity:      Option<SourceId>,
    lock_identity: Option<Identity>,
    lm:            Option<u64>,
    /// [`None`] for a pin with no lock entry, which dedup never fetches
    source:        Option<SourceRef>,
    submodules:    bool,
    pin_type:      PinType,
    omit:          OmitPolicy,
    follow:        FollowPolicy,
}

struct ScanOutcome {
    groups:      BTreeMap<SourceId, Vec<Entry>>,
    diagnostics: Vec<ScanDiagnostic>,
    errors:      Vec<(Vec<String>, String)>,
}

impl TargetPin {
    fn declared(
        input: &pins::Input,
        expanded: &str,
        node: Option<&LockedNode>,
        path: Vec<String>,
        omit: OmitPolicy,
        follow: FollowPolicy,
    ) -> Self {
        Self {
            path,
            identity: node
                .and_then(SourceId::from_locked)
                .or_else(|| SourceId::from_url(expanded)),
            lock_identity: node
                .and_then(LockedNode::source_identity)
                .map(Identity::from_lock),
            lm: node.and_then(LockedNode::last_modified),
            source: node.cloned().map(SourceRef::Locked),
            submodules: input.submodules,
            pin_type: input.pin_type,
            omit,
            follow,
        }
    }
}

fn target_registry(
    inputs: &[pins::Input],
    lock: &LockFile,
    shorturls: &ShortUrls<'_>,
    omit_inputs: &BTreeSet<String>,
    all_follow: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, TargetPin>> {
    let mut targets = inputs
        .iter()
        .map(|input| {
            let expanded = shorturls.expand(&input.url)?;
            let target = TargetPin::declared(
                input,
                &expanded,
                lock.get(&input.name),
                vec![input.name.clone()],
                OmitPolicy::for_input(omit_inputs, input),
                FollowPolicy::for_input(all_follow, input),
            );
            Ok((input.name.clone(), target))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;

    for name in all_follow.values() {
        if targets.contains_key(name) {
            continue;
        }
        let Some(node) = lock.get(name) else {
            continue;
        };
        targets.insert(name.clone(), TargetPin {
            path:          vec![name.clone()],
            identity:      SourceId::from_locked(node),
            lock_identity: node.source_identity().map(Identity::from_lock),
            lm:            node.last_modified(),
            source:        Some(SourceRef::Locked(node.clone())),
            submodules:    false,
            pin_type:      PinType::Flake,
            omit:          OmitPolicy::synthetic(omit_inputs),
            follow:        FollowPolicy::synthetic(all_follow, name),
        });
    }
    Ok(targets)
}

fn coalesce_groups(groups: &mut BTreeMap<SourceId, Vec<Entry>>) {
    for entries in groups.values_mut() {
        let mut unique = BTreeMap::<(Vec<String>, Side, String, Option<Identity>), Entry>::new();
        for entry in mem::take(entries) {
            let key = (
                entry.path.clone(),
                entry.side,
                entry.name.clone(),
                entry.identity.clone(),
            );
            unique
                .entry(key)
                .and_modify(|existing| existing.lm = existing.lm.max(entry.lm))
                .or_insert(entry);
        }
        *entries = unique.into_values().collect();
    }
}

fn queue_followed(
    followed: FollowedInput,
    targets: &BTreeMap<String, TargetPin>,
    ancestry: &BTreeSet<String>,
    groups: &mut BTreeMap<SourceId, Vec<Entry>>,
) -> Result<Option<ScanTarget>, ScanDiagnostic> {
    // `pin/input` reuses an input the named pin's own scan already records
    let bare = followed
        .target
        .rsplit("::")
        .next()
        .unwrap_or(&followed.target);
    if bare.contains('/') {
        return Ok(None);
    }
    let Some(follow_target) = targets.get(&followed.target) else {
        // the rule lives in the pins.toml of the project its namespace names
        let declared = followed
            .target
            .rsplit_once("::")
            .map(|(namespace, _)| namespace.split('/').map(str::to_owned).collect::<Vec<_>>())
            .unwrap_or_default();
        return Err(ScanDiagnostic::config(
            &declared,
            ScanFile::TackPins,
            format!(
                "follow target '{}' is not a locked or declared input",
                followed.target
            ),
        ));
    };
    if let Some(identity) = follow_target.identity.as_ref() {
        groups.entry(identity.clone()).or_default().push(Entry {
            path:     followed.path,
            name:     followed.name,
            side:     followed.side,
            identity: follow_target.lock_identity.clone(),
            lm:       follow_target.lm,
        });
    }
    let Some(ref source) = follow_target.source else {
        return Ok(None);
    };
    Ok((follow_target.pin_type != PinType::Fixed).then(|| {
        ScanTarget {
            path:       follow_target.path.clone(),
            source:     source.clone(),
            submodules: follow_target.submodules,
            pin_type:   follow_target.pin_type,
            omit:       follow_target.omit.clone(),
            follow:     follow_target.follow.clone(),
            ancestors:  ancestry.clone(),
        }
    }))
}

/// everything a scan's output depends on apart from the route's path
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ScanContext {
    key:      String,
    pin_type: &'static str,
    omit:     OmitPolicy,
    follow:   FollowPolicy,
}

impl ScanContext {
    fn of(target: &ScanTarget) -> Self {
        Self {
            key:      target.key(),
            pin_type: target.pin_type.as_str(),
            omit:     target.omit.clone(),
            follow:   target.follow.clone(),
        }
    }
}

#[derive(Default)]
struct RouteState {
    targets:     BTreeMap<String, TargetPin>,
    documents:   BTreeMap<String, ScanDocuments>,
    failed:      BTreeSet<String>,
    expanded:    BTreeSet<ScanContext>,
    routed:      BTreeSet<(ScanContext, Vec<String>)>,
    groups:      BTreeMap<SourceId, Vec<Entry>>,
    diagnostics: Vec<ScanDiagnostic>,
    errors:      Vec<(Vec<String>, String)>,
}

impl RouteState {
    fn admit(&mut self, frontier: Vec<ScanTarget>) -> Vec<ScanTarget> {
        let mut active = Vec::new();
        for target in frontier {
            let key = target.key();
            if target.ancestors.contains(&key) || self.failed.contains(&key) {
                continue;
            }
            if target.path.len() > MAX_ROUTE_DEPTH {
                self.diagnostics.push(ScanDiagnostic::truncated(
                    &target.path,
                    ScanFile::TackPins,
                    format!(
                        "pins nest deeper than {MAX_ROUTE_DEPTH} projects, deeper ones were \
                         skipped"
                    ),
                ));
                continue;
            }
            if self.routed.len() >= MAX_ROUTES {
                self.diagnostics.push(ScanDiagnostic::truncated(
                    &[],
                    ScanFile::TackPins,
                    format!("more than {MAX_ROUTES} scans, the rest were skipped"),
                ));
                break;
            }
            if self
                .routed
                .insert((ScanContext::of(&target), target.path.clone()))
            {
                active.push(target);
            }
        }
        active.sort_by(|left, right| left.path.cmp(&right.path));
        active
    }

    fn load(&mut self, active: &[ScanTarget]) {
        let mut jobs = BTreeMap::<String, ScanTarget>::new();
        for target in active {
            let key = target.key();
            if !self.documents.contains_key(&key) {
                jobs.entry(key).or_insert_with(|| target.clone());
            }
        }
        let loads = jobs.into_values().collect::<Vec<_>>();
        let load_results = dispatcher::ordered(loads, DEDUP_SCAN_IN_FLIGHT, |_, target| {
            let key = target.key();
            let path = target.path.clone();
            (key, path, target.load_documents())
        });
        for (key, path, result) in load_results {
            match result {
                Ok(load_result) => {
                    self.diagnostics.extend(load_result.diagnostics);
                    self.documents.insert(key, load_result.documents);
                },
                Err(err) => {
                    self.failed.insert(key);
                    self.errors.push((path, format!("{err:#}")));
                },
            }
        }
    }

    fn scan(&mut self, target: &ScanTarget, frontier: &mut Vec<ScanTarget>) {
        let target_key = target.key();
        let Some(route_documents) = self.documents.get(&target_key) else {
            return;
        };
        let scan =
            route_documents.scan(&target.path, &target.omit, &target.follow, target.pin_type);
        // the first route into a context expands it, later routes only add
        // their own occurrences, so shared subgraphs are walked once
        let expand = self.expanded.insert(ScanContext::of(target));
        let mut ancestry = target.ancestors.clone();
        ancestry.insert(target_key);
        if expand {
            self.diagnostics.extend(scan.diagnostics);
            for (name, registration) in scan.registry {
                let pin = match registration {
                    Registration::Pin(pin) => *pin,
                    Registration::Alias(aliased) => {
                        let Some(pin) = self.targets.get(&aliased) else {
                            continue;
                        };
                        pin.clone()
                    },
                };
                self.targets.insert(name, pin);
            }
            for mut transitive in scan.transitive {
                transitive.ancestors.clone_from(&ancestry);
                frontier.push(transitive);
            }
        }
        for finding in scan.findings {
            self.groups
                .entry(finding.identity)
                .or_default()
                .push(finding.entry);
        }
        for followed in scan.followed {
            match queue_followed(followed, &self.targets, &ancestry, &mut self.groups) {
                Ok(Some(next)) if expand => frontier.push(next),
                Err(diagnostic) if expand => self.diagnostics.push(diagnostic),
                Ok(_) | Err(_) => {},
            }
        }
    }
}

fn scan_routes(mut frontier: Vec<ScanTarget>, roots: &BTreeMap<String, TargetPin>) -> ScanOutcome {
    // nested docs register their own follow targets as scanning discovers them
    let mut state = RouteState {
        targets: roots.clone(),
        ..RouteState::default()
    };

    while !frontier.is_empty() {
        let active = state.admit(mem::take(&mut frontier));
        state.load(&active);
        for target in &active {
            state.scan(target, &mut frontier);
        }
    }
    let RouteState {
        groups,
        mut diagnostics,
        mut errors,
        ..
    } = state;
    diagnostics.sort();
    diagnostics.dedup();
    errors.sort();
    errors.dedup();
    ScanOutcome {
        groups,
        diagnostics,
        errors,
    }
}

pub fn dedup(project: &Project) -> Result<()> {
    let report = dedup_report_inner(project, true)?;
    render::print_report(&report);
    Ok(())
}

pub fn dedup_report(project: &Project) -> Result<DedupReport> {
    dedup_report_inner(project, false)
}

fn dedup_report_inner(project: &Project, emit_diagnostics: bool) -> Result<DedupReport> {
    let doc = project.load_pins()?;
    let lock = project.load_lock()?;
    let inputs = doc.inputs()?;
    let shorturls = doc.shorturls();
    let all_follow = doc.all_follows()?;
    let omit_inputs = doc.omit_inputs()?;
    let top_revs = top_map(&inputs, &lock, |node| {
        node.source_identity().map(Identity::from_lock)
    });
    let targets = target_registry(&inputs, &lock, &shorturls, &omit_inputs, &all_follow)?;
    let mut groups = BTreeMap::<SourceId, Vec<Entry>>::new();

    for input in &inputs {
        let expanded = shorturls.expand(&input.url)?;
        if let Some(id) = SourceId::from_url(&expanded) {
            let identity = top_revs.get(&input.name).cloned();
            let lm = lock.get(&input.name).and_then(LockedNode::last_modified);
            groups.entry(id).or_default().push(Entry {
                path: vec![],
                name: input.name.clone(),
                side: Side::Flake,
                identity,
                lm,
            });
        }
    }

    let frontier = inputs
        .iter()
        .filter_map(|input| {
            if input.pin_type == PinType::Fixed {
                return None;
            }
            let node = lock.get(&input.name)?;
            Some(ScanTarget {
                path:       vec![input.name.clone()],
                source:     SourceRef::Locked(node.clone()),
                submodules: input.submodules,
                pin_type:   input.pin_type,
                omit:       OmitPolicy::for_input(&omit_inputs, input),
                follow:     FollowPolicy::for_input(&all_follow, input),
                ancestors:  BTreeSet::new(),
            })
        })
        .collect::<Vec<_>>();
    if emit_diagnostics {
        eprintln!("scanning {} pin(s)...", frontier.len());
    }

    let scanned = scan_routes(frontier, &targets);
    for (id, entries) in scanned.groups {
        groups.entry(id).or_default().extend(entries);
    }
    coalesce_groups(&mut groups);
    if emit_diagnostics {
        for diagnostic in scanned.diagnostics {
            eprintln!("tack: {}", render::scan_diagnostic(&diagnostic));
        }
        for (path, err) in scanned.errors {
            eprintln!("tack: {}", render::scan_error(&path, &err));
        }
    }

    let AheadBehindResult {
        compares,
        surfaced_causes,
        dropped,
    } = ahead_behind(&groups);
    if emit_diagnostics {
        for cause in &surfaced_causes {
            eprintln!("tack: {}", render::printable(cause));
        }
        if dropped > 0 {
            eprintln!(
                "tack: {dropped} branch comparison(s) unavailable or capped; falling back to \
                 commit-date order"
            );
        }
    }
    Ok(build_report(&groups, &all_follow, &compares))
}
