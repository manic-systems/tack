// SPDX-License-Identifier: EUPL-1.2

mod auto;
mod compare;
mod follows;
mod model;
mod reporting;
mod scan;

use std::{
    collections::{
        BTreeMap,
        HashSet,
    },
    mem,
};

use eyre::Result;

pub(super) use self::auto::{
    AutoDedupReport,
    auto_dedup_scoped,
};
use self::{
    compare::ahead_behind,
    follows::apply_follows,
    model::{
        Entry,
        Side,
    },
    reporting::build_report,
    scan::{
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
        .collect::<HashSet<&str>>();
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
    let by_name = inputs
        .iter()
        .map(|inp| (inp.name.as_str(), inp))
        .collect::<BTreeMap<&str, &pins::Input>>();

    let top_revs = top_map(&inputs, &lock, |n| {
        n.source_identity()
            .map(LockIdentity::as_str)
            .map(str::to_owned)
    });
    let top_lms = top_map(&inputs, &lock, LockedNode::last_modified);

    let mut groups = BTreeMap::<SourceId, Vec<Entry>>::new();

    for inp in &inputs {
        let expanded = shorturls.expand(&inp.url);
        if let Some(id) = SourceId::from_url(&expanded) {
            let rev = top_revs.get(&inp.name).cloned().unwrap_or_default();
            let lm = lock.get(&inp.name).and_then(LockedNode::last_modified);
            groups.entry(id).or_default().push(Entry {
                path: vec![],
                name: inp.name.clone(),
                side: Side::Flake,
                rev,
                lm,
            });
        }
    }

    let mut frontier = inputs
        .iter()
        .filter_map(|inp| {
            if inp.pin_type != PinType::Flake {
                return None;
            }
            let node = lock.get(&inp.name)?;
            Some(ScanTarget {
                path:       vec![inp.name.clone()],
                source:     SourceRef::Locked(node.clone()),
                submodules: inp.submodules,
            })
        })
        .collect::<Vec<ScanTarget>>();
    if emit_diagnostics {
        eprintln!("scanning {} pin(s)...", frontier.len());
    }

    // breadth first so `visited` cuts cycles before the next batch
    let mut visited = HashSet::<String>::new();
    while !frontier.is_empty() {
        let scan_jobs = mem::take(&mut frontier)
            .into_iter()
            .filter(|item| visited.insert(item.source.key()))
            .collect::<Vec<_>>();
        let results = dispatcher::ordered(scan_jobs, DEDUP_SCAN_IN_FLIGHT, |_, item| {
            (item.path.clone(), item.fetch_and_scan())
        });

        for (path, res) in results {
            match res {
                Ok(scan) => {
                    if emit_diagnostics {
                        for diagnostic in scan.diagnostics {
                            eprintln!("tack: {}", render::scan_diagnostic(&diagnostic));
                        }
                    }
                    for finding in scan.findings {
                        groups
                            .entry(finding.identity)
                            .or_default()
                            .push(finding.entry);
                    }
                    frontier.extend(scan.transitive);
                },
                Err(err) => {
                    if emit_diagnostics {
                        eprintln!("tack: scan {}: {err:#}", render::source_label(&path));
                    }
                },
            }
        }
    }

    apply_follows(&mut groups, &by_name, &all_follow, &top_revs, &top_lms);

    let compares = ahead_behind(&groups);
    Ok(build_report(&groups, &all_follow, &compares))
}
