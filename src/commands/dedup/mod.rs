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

use eyre::Result;

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
        Side,
    },
    reporting::build_report,
    scan::{
        FollowPolicy,
        FollowedInput,
        LoadedDocuments,
        OmitPolicy,
        ScanDocuments,
        ScanTarget,
        SourceRef,
    },
};
use crate::{
    dispatcher,
    lock::{
        LockFile,
        LockIdentity,
        LockedNode,
    },
    pins::{
        self,
        PinType,
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
    identity:   Option<SourceId>,
    rev:        String,
    lm:         Option<u64>,
    source:     SourceRef,
    submodules: bool,
    pin_type:   PinType,
    omit:       OmitPolicy,
    follow:     FollowPolicy,
}

struct ScanOutcome {
    groups:      BTreeMap<SourceId, Vec<Entry>>,
    diagnostics: Vec<ScanDiagnostic>,
    errors:      Vec<(Vec<String>, String)>,
}

fn target_pin(
    input: &pins::Input,
    expanded: &str,
    node: Option<&LockedNode>,
    omit_inputs: &BTreeSet<String>,
    all_follow: &BTreeMap<String, String>,
) -> TargetPin {
    let source = node
        .cloned()
        .map_or_else(|| SourceRef::Url(expanded.to_owned()), SourceRef::Locked);
    TargetPin {
        identity: node
            .and_then(SourceId::from_locked)
            .or_else(|| SourceId::from_url(expanded)),
        rev: node
            .and_then(|locked| locked.source_identity().map(LockIdentity::into_string))
            .unwrap_or_default(),
        lm: node.and_then(LockedNode::last_modified),
        source,
        submodules: input.submodules,
        pin_type: input.pin_type,
        omit: OmitPolicy::for_input(omit_inputs, input),
        follow: FollowPolicy::for_input(all_follow, input),
    }
}

fn target_registry(
    inputs: &[pins::Input],
    lock: &LockFile,
    shorturls: &ShortUrls<'_>,
    omit_inputs: &BTreeSet<String>,
    all_follow: &BTreeMap<String, String>,
) -> BTreeMap<String, TargetPin> {
    let mut targets = inputs
        .iter()
        .map(|input| {
            let expanded = shorturls.expand(&input.url);
            let target = target_pin(
                input,
                &expanded,
                lock.get(&input.name),
                omit_inputs,
                all_follow,
            );
            (input.name.clone(), target)
        })
        .collect::<BTreeMap<_, _>>();

    for name in all_follow.values() {
        if targets.contains_key(name) {
            continue;
        }
        let Some(node) = lock.get(name) else {
            continue;
        };
        targets.insert(name.clone(), TargetPin {
            identity:   SourceId::from_locked(node),
            rev:        node
                .source_identity()
                .map(LockIdentity::into_string)
                .unwrap_or_default(),
            lm:         node.last_modified(),
            source:     SourceRef::Locked(node.clone()),
            submodules: false,
            pin_type:   PinType::Flake,
            omit:       OmitPolicy::synthetic(omit_inputs),
            follow:     FollowPolicy::synthetic(all_follow),
        });
    }
    targets
}

fn coalesce_groups(groups: &mut BTreeMap<SourceId, Vec<Entry>>) {
    for entries in groups.values_mut() {
        let mut unique = BTreeMap::<(Vec<String>, Side, String, String), Entry>::new();
        for entry in mem::take(entries) {
            let key = (
                entry.path.clone(),
                entry.side,
                entry.name.clone(),
                entry.rev.clone(),
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
    frontier: &mut Vec<ScanTarget>,
    diagnostics: &mut Vec<ScanDiagnostic>,
) {
    let Some(follow_target) = targets.get(&followed.target) else {
        diagnostics.push(ScanDiagnostic::config(
            &followed.path,
            ScanFile::TackPins,
            format!(
                "follow target '{}' is not a locked or declared input",
                followed.target
            ),
        ));
        return;
    };
    if let Some(identity) = follow_target.identity.as_ref() {
        groups.entry(identity.clone()).or_default().push(Entry {
            path: followed.path.clone(),
            name: followed.name.clone(),
            side: followed.side,
            rev:  follow_target.rev.clone(),
            lm:   follow_target.lm,
        });
    }
    if follow_target.pin_type != PinType::Fixed {
        let mut path = followed.path;
        path.push(followed.name);
        frontier.push(ScanTarget {
            path,
            source: follow_target.source.clone(),
            submodules: follow_target.submodules,
            pin_type: follow_target.pin_type,
            omit: follow_target.omit.clone(),
            follow: follow_target.follow.clone(),
            ancestors: ancestry.clone(),
        });
    }
}

fn scan_routes<L>(
    mut frontier: Vec<ScanTarget>,
    roots: &BTreeMap<String, TargetPin>,
    loader: &L,
) -> ScanOutcome
where
    L: Fn(&ScanTarget) -> Result<LoadedDocuments> + Sync,
{
    // nested docs register their own follow targets as scanning discovers them
    let mut targets = roots.clone();
    let mut documents = BTreeMap::<String, ScanDocuments>::new();
    let mut groups = BTreeMap::<SourceId, Vec<Entry>>::new();
    let mut diagnostics = Vec::new();
    let mut errors = Vec::new();

    while !frontier.is_empty() {
        let active = mem::take(&mut frontier)
            .into_iter()
            .filter(|target| !target.ancestors.contains(&target.key()))
            .collect::<Vec<_>>();
        let loads = active
            .iter()
            .filter(|target| !documents.contains_key(&target.key()))
            .fold(BTreeMap::<String, ScanTarget>::new(), |mut jobs, target| {
                jobs.entry(target.key())
                    .and_modify(|selected| {
                        if target.path < selected.path {
                            selected.clone_from(target);
                        }
                    })
                    .or_insert_with(|| target.clone());
                jobs
            })
            .into_values()
            .collect::<Vec<_>>();
        let load_results = dispatcher::ordered(loads, DEDUP_SCAN_IN_FLIGHT, |_, target| {
            let key = target.key();
            let path = target.path.clone();
            (key, path, loader(&target))
        });
        for (key, path, result) in load_results {
            match result {
                Ok(load_result) => {
                    diagnostics.extend(load_result.diagnostics);
                    documents.insert(key, load_result.documents);
                },
                Err(err) => errors.push((path, format!("{err:#}"))),
            }
        }

        for target in active {
            let target_key = target.key();
            let Some(route_documents) = documents.get(&target_key) else {
                continue;
            };
            let mut ancestry = target.ancestors.clone();
            ancestry.insert(target_key);
            let scan =
                route_documents.scan(&target.path, &target.omit, &target.follow, target.pin_type);
            diagnostics.extend(scan.diagnostics);
            targets.extend(scan.registry);
            for finding in scan.findings {
                groups
                    .entry(finding.identity)
                    .or_default()
                    .push(finding.entry);
            }
            for mut transitive in scan.transitive {
                transitive.ancestors.clone_from(&ancestry);
                frontier.push(transitive);
            }
            for followed in scan.followed {
                queue_followed(
                    followed,
                    &targets,
                    &ancestry,
                    &mut groups,
                    &mut frontier,
                    &mut diagnostics,
                );
            }
        }
    }
    coalesce_groups(&mut groups);
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
        node.source_identity().map(LockIdentity::into_string)
    });
    let targets = target_registry(&inputs, &lock, &shorturls, &omit_inputs, &all_follow);
    let mut groups = BTreeMap::<SourceId, Vec<Entry>>::new();

    for input in &inputs {
        let expanded = shorturls.expand(&input.url);
        if let Some(id) = SourceId::from_url(&expanded) {
            let rev = top_revs.get(&input.name).cloned().unwrap_or_default();
            let lm = lock.get(&input.name).and_then(LockedNode::last_modified);
            groups.entry(id).or_default().push(Entry {
                path: vec![],
                name: input.name.clone(),
                side: Side::Flake,
                rev,
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

    let scanned = scan_routes(frontier, &targets, &ScanTarget::load_documents);
    for (id, entries) in scanned.groups {
        groups.entry(id).or_default().extend(entries);
    }
    coalesce_groups(&mut groups);
    if emit_diagnostics {
        for diagnostic in scanned.diagnostics {
            eprintln!("tack: {}", render::scan_diagnostic(&diagnostic));
        }
        for (path, err) in scanned.errors {
            eprintln!("tack: scan {}: {err}", render::source_label(&path));
        }
    }

    let AheadBehindResult {
        compares,
        surfaced_causes,
        dropped,
    } = ahead_behind(&groups);
    if emit_diagnostics {
        for cause in &surfaced_causes {
            eprintln!("tack: {cause}");
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

#[cfg(test)]
#[path = "orchestration_tests.rs"]
mod tests;
