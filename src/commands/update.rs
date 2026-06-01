// SPDX-License-Identifier: EUPL-1.2

use std::{
    iter,
    sync::{
        Mutex,
        atomic::{
            AtomicUsize,
            Ordering,
        },
    },
};

use eyre::{
    Result,
    bail,
};
use rayon::prelude::{
    IndexedParallelIterator as _,
    IntoParallelRefIterator as _,
    ParallelIterator as _,
};

use super::{
    dedup,
    select,
};
use crate::{
    fetch::{
        self,
        github::BranchComparison,
    },
    lock::LockedNode,
    pins::{
        self,
        PinType,
    },
    project::Project,
    render,
    source::Source,
    ui::{
        Display,
        PinStatus,
    },
};

struct UpdateFetch {
    node:       LockedNode,
    rev:        String,
    comparison: BranchComparison,
}

impl UpdateFetch {
    fn fetch(input: &pins::Input, expanded: &str, old_rev: Option<&str>) -> Result<Self> {
        match input.pin_type {
            PinType::Fixed => {
                fetch::fetch_fixed_pin(expanded, input.unpack).map(|(node, rev)| {
                    Self {
                        node,
                        rev,
                        comparison: BranchComparison::none(),
                    }
                })
            },
            PinType::Flake | PinType::Fetch => {
                let source = expanded.parse::<Source>()?;
                fetch::fetch_pin_compared(&source, input.submodules, old_rev).map(|fetched| {
                    Self {
                        node:       fetched.node,
                        rev:        fetched.rev,
                        comparison: fetched.comparison,
                    }
                })
            },
        }
    }
}

struct UpdateRunner<'a> {
    accept:  bool,
    display: &'a Display,
    drift:   &'a AtomicUsize,
}

impl<'a> UpdateRunner<'a> {
    const fn new(accept: bool, display: &'a Display, drift: &'a AtomicUsize) -> Self {
        Self {
            accept,
            display,
            drift,
        }
    }

    fn update_one(
        &self,
        index: usize,
        input: &pins::Input,
        expanded: &str,
        old: Option<&LockedNode>,
    ) -> Option<LockedNode> {
        self.display.set(index, PinStatus::Fetching { frame: 0 });
        let old_rev = old.and_then(LockedNode::rev);
        match UpdateFetch::fetch(input, expanded, old_rev) {
            // for fixed pins sha256 is the identity, so any mismatch is drift
            Ok(UpdateFetch { node, rev, .. })
                if input.pin_type == PinType::Fixed
                    && old_rev.is_some()
                    && old_rev != Some(rev.as_str()) =>
            {
                self.display.set(index, PinStatus::FixedDrift {
                    old:      old_rev.map(render::short).unwrap_or_default(),
                    new:      render::short(&rev),
                    accepted: self.accept,
                });
                self.accept_or_record_drift(node)
            },
            Ok(UpdateFetch { node, rev, .. }) if old_rev == Some(rev.as_str()) => {
                // same rev, if hash moved, upstream changed under a stable rev
                if Self::hash_drifted(old, &node) {
                    self.display.set(index, PinStatus::Drift {
                        rev:      render::short(&rev),
                        accepted: self.accept,
                    });
                    self.accept_or_record_drift(node)
                } else {
                    self.display.set(index, PinStatus::NoChange);
                    None
                }
            },
            Ok(UpdateFetch {
                node,
                rev,
                comparison,
            }) => {
                self.display.set(index, PinStatus::Updated {
                    old: old_rev.map_or_else(|| "NEW".into(), render::short),
                    new: render::short(&rev),
                    comparison,
                });
                Some(node)
            },
            Err(err) => {
                self.display
                    .set(index, PinStatus::Failed(format!("{err:#}")));
                None
            },
        }
    }

    fn accept_or_record_drift(&self, node: LockedNode) -> Option<LockedNode> {
        if self.accept {
            Some(node)
        } else {
            self.drift.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    fn hash_drifted(old: Option<&LockedNode>, node: &LockedNode) -> bool {
        matches!(
            (old.and_then(LockedNode::hash), node.hash()),
            (Some(prev), Some(curr)) if prev != curr
        )
    }
}

pub fn update(project: &Project, names: &[String], accept: bool) -> Result<()> {
    let doc = project.load_pins()?;
    let shorturls = doc.shorturls();
    let all = doc.inputs()?;
    let all_follow = doc.all_follows()?;
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(());
    }
    let mut lk = project.load_lock()?;

    let display = Display::new(selected.iter().map(|i| i.name.clone()).collect());
    let drift = AtomicUsize::new(0);
    let runner = UpdateRunner::new(accept, &display, &drift);

    let results = selected
        .par_iter()
        .enumerate()
        .map(|(i, inp)| {
            let expanded = shorturls.expand(&inp.url);
            let old = lk.get(&inp.name);
            runner.update_one(i, inp, &expanded, old)
        })
        .collect::<Vec<Option<LockedNode>>>();

    let mut changed = false;
    for (inp, result) in selected.iter().zip(results) {
        if let Some(node) = result {
            lk.insert(inp.name.clone(), node);
            changed = true;
        }
    }
    let drift_count = drift.load(Ordering::Relaxed);
    let auto_dedup = if drift_count == 0 {
        dedup::auto_dedup(&all, &all_follow, &mut lk)
    } else {
        dedup::AutoDedupReport::default()
    };
    if auto_dedup.changed {
        changed = true;
    }
    if changed {
        project.save_lock(&lk)?;
    }
    display.finish();
    for diagnostic in auto_dedup.scan_diagnostics {
        eprintln!("tack: {}", render::scan_diagnostic(&diagnostic));
    }
    for cause in auto_dedup.surfaced_fetch_causes {
        eprintln!("tack: {cause}");
    }

    if drift_count > 0 {
        bail!(
            "upstream content differs from lock (drifted pins kept; investigate, then re-run with \
             --accept to relock)"
        );
    }
    Ok(())
}

pub fn look(project: &Project, names: &[String], verbose: bool) -> Result<()> {
    const LOG_LIMIT: usize = 5;

    let doc = project.load_pins()?;
    let shorturls = doc.shorturls();
    let all = doc.inputs()?;
    if all.is_empty() {
        println!(
            "no pins in {}; add one with `tack add <name> <url>`",
            project.pins_path().display()
        );
        return Ok(());
    }
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(());
    }
    let lk = project.load_lock()?;

    let display = Display::new(selected.iter().map(|inp| inp.name.clone()).collect());
    let logs: Vec<Mutex<Option<fetch::github::CommitLog>>> = iter::repeat_with(|| Mutex::new(None))
        .take(selected.len())
        .collect();

    selected.par_iter().enumerate().for_each(|(i, inp)| {
        if inp.pin_type == PinType::Fixed {
            display.set(
                i,
                PinStatus::Skipped("fixed pin, run `tack update` to verify".into()),
            );
            return;
        }
        display.set(i, PinStatus::Fetching { frame: 0 });
        let expanded = shorturls.expand(&inp.url);
        let source = match expanded.parse::<Source>() {
            Ok(source) => source,
            Err(err) => {
                display.set(i, PinStatus::Failed(format!("{err:#}")));
                return;
            },
        };
        let old = lk
            .get(&inp.name)
            .and_then(LockedNode::rev)
            .map(str::to_owned);
        match fetch::current_rev_compared(&source, old.as_deref()) {
            Ok(current) if old.as_deref() == Some(current.rev.as_str()) => {
                display.set(i, PinStatus::NoChange);
            },
            Ok(current) => {
                display.set(i, PinStatus::Updated {
                    old:        old.as_deref().map_or_else(|| "NEW".into(), render::short),
                    new:        render::short(&current.rev),
                    comparison: current.comparison,
                });
                // commit logs are adjunct to the already-rendered rev status,
                // so keep them best-effort rather than surfacing fetch probes
                // while the spinner owns the display
                if verbose
                    && let Some(old_rev) = old.as_deref()
                    && let Ok(Some(log)) =
                        fetch::github::commits_between(&source, old_rev, &current.rev, LOG_LIMIT)
                {
                    *logs[i].lock().unwrap() = Some(log);
                }
            },
            Err(err) => display.set(i, PinStatus::Failed(format!("{err:#}"))),
        }
    });

    if verbose {
        let collected = logs
            .into_iter()
            .map(|mutex| mutex.into_inner().unwrap())
            .collect::<Vec<_>>();
        display.finish_verbose(&collected);
    } else {
        display.finish();
    }
    Ok(())
}
