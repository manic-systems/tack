// SPDX-License-Identifier: EUPL-1.2

use std::{
    cmp,
    collections::{
        BTreeMap,
        HashMap,
        HashSet,
    },
};

use super::model::Entry;
use crate::{
    fetch::{
        CompareStatus,
        compare_planner::{
            CompareJob as PlannerCompareJob,
            CompareSession,
            CompareSource,
        },
    },
    report::Mark,
    source::id::SourceId,
};

pub(super) const MAX_COMPARE_JOBS: usize = 100;
const MAX_LIVE_COMPARE_JOBS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CompareWork {
    pub id:   SourceId,
    pub head: String,
    pub job:  PlannerCompareJob,
}

/// declared pin then newest lock then name order
pub(super) fn comparator(entries: &[Entry]) -> Option<&Entry> {
    entries
        .iter()
        .filter(|entry| entry.path.is_empty())
        .min_by_key(|entry| entry.name.as_str())
        .or_else(|| {
            entries
                .iter()
                .filter(|entry| entry.lm.is_some())
                .max_by_key(|entry| entry.lm)
        })
        .or_else(|| entries.iter().min_by_key(|entry| entry.name.as_str()))
}

pub(super) fn group_diverges(entries: &[Entry]) -> bool {
    let mut revs = entries.iter().map(entry_compare_rev);
    revs.next()
        .is_some_and(|first| revs.any(|rev| rev != first))
}

pub(super) const fn entry_compare_rev(entry: &Entry) -> &str {
    entry.rev.as_str()
}

pub(super) fn compare_jobs(groups: &BTreeMap<SourceId, Vec<Entry>>) -> (Vec<CompareWork>, usize) {
    let mut jobs = groups
        .iter()
        .filter(|group| group_diverges(group.1))
        .filter_map(|(id, entries)| {
            let base = comparator(entries)?;
            if base.rev.is_empty() || CompareSource::from_source_id(id).is_none() {
                return None;
            }
            let mut seen = HashSet::new();
            let heads = entries
                .iter()
                .filter(|entry| {
                    entry.rev != base.rev
                        && !entry.rev.is_empty()
                        && seen.insert(entry.rev.as_str())
                })
                .filter_map(|entry| {
                    PlannerCompareJob::from_source_id(id, &base.rev, &entry.rev).map(|job| {
                        CompareWork {
                            id: id.clone(),
                            head: entry.rev.clone(),
                            job,
                        }
                    })
                })
                .collect::<Vec<_>>();
            Some(heads)
        })
        .flatten()
        .collect::<Vec<_>>();

    let capped = jobs.len().saturating_sub(MAX_COMPARE_JOBS);
    jobs.truncate(MAX_COMPARE_JOBS);
    (jobs, capped)
}

pub(super) fn ahead_behind(
    groups: &BTreeMap<SourceId, Vec<Entry>>,
) -> HashMap<(SourceId, String), CompareStatus> {
    let (jobs, capped) = compare_jobs(groups);
    let attempted = jobs.len();
    let mut compares = HashMap::<(SourceId, String), CompareStatus>::new();
    let session = CompareSession::new();
    let planner_jobs = jobs.iter().map(|work| work.job.clone()).collect::<Vec<_>>();
    let results = session.compare_batch(planner_jobs, MAX_LIVE_COMPARE_JOBS);
    for (index, work) in jobs.iter().enumerate() {
        if let Some(status) = results
            .get(index)
            .and_then(|attempt| attempt.as_ref())
            .and_then(|attempt| attempt.status)
        {
            compares.insert((work.id.clone(), work.head.clone()), status);
        }
    }

    for cause in session.into_surfaced() {
        eprintln!("tack: {cause}");
    }
    let dropped = capped + attempted - compares.len();
    if dropped > 0 {
        eprintln!(
            "tack: {dropped} branch comparison(s) unavailable or capped; falling back to \
             commit-date order"
        );
    }
    compares
}

pub(super) fn rev_last_modified(entries: &[Entry]) -> BTreeMap<&str, u64> {
    let mut lm_of = BTreeMap::<&str, u64>::new();
    for entry in entries {
        let Some(lm) = entry.lm else {
            continue;
        };
        let slot = lm_of.entry(entry_compare_rev(entry)).or_insert(lm);
        *slot = (*slot).max(lm);
    }
    lm_of
}

pub(super) fn classify(
    id: &SourceId,
    rev: &str,
    comparator: Option<&Entry>,
    lm_of: &BTreeMap<&str, u64>,
    compares: &HashMap<(SourceId, String), CompareStatus>,
) -> Mark {
    let Some(comp) = comparator else {
        return Mark::Unknown;
    };
    if rev == entry_compare_rev(comp) {
        return Mark::Base;
    }
    if let Some(status) = compares.get(&(id.clone(), rev.to_owned())) {
        return match *status {
            CompareStatus::Ahead => Mark::Ahead,
            CompareStatus::Behind => Mark::Behind,
            CompareStatus::Diverged => Mark::Diverged,
            CompareStatus::Identical => Mark::Base,
        };
    }
    let (Some(comp_lm), Some(lm)) = (comp.lm, lm_of.get(rev).copied()) else {
        return Mark::Unknown;
    };
    match lm.cmp(&comp_lm) {
        cmp::Ordering::Equal => Mark::DatedEqual,
        cmp::Ordering::Greater => Mark::DatedNewer,
        cmp::Ordering::Less => Mark::DatedOlder,
    }
}

#[cfg(test)]
#[path = "compare_tests.rs"]
mod tests;
