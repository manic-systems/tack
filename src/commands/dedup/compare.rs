// SPDX-License-Identifier: EUPL-1.2

use std::{
    cmp,
    collections::{
        BTreeMap,
        HashMap,
        HashSet,
    },
};

use rayon::prelude::{
    IntoParallelIterator as _,
    ParallelIterator as _,
};

use super::{
    forge_compare::{
        self,
        SurfacedCauses,
    },
    model::Entry,
};
use crate::{
    fetch::github::CompareStatus,
    report::Mark,
    source::id::SourceId,
};

pub(super) const MAX_COMPARE_JOBS: usize = 100;
const MAX_LIVE_COMPARE_JOBS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CompareJob {
    pub id:   SourceId,
    pub base: String,
    pub head: String,
}

/// the entry a group is measured against: top pin, else newest transitive by
/// `lastModified`, else lowest-named for determinism
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

/// worth printing only when revs disagree
pub(super) fn group_diverges(entries: &[Entry]) -> bool {
    let mut revs = entries.iter().map(entry_compare_rev);
    revs.next()
        .is_some_and(|first| revs.any(|rev| rev != first))
}

pub(super) const fn entry_compare_rev(entry: &Entry) -> &str {
    entry.rev.as_str()
}

/// forge compare work for each divergent rev vs its comparator
/// jobs carry full revs (request + map keys); abbreviation happens at render
pub(super) fn compare_jobs(groups: &BTreeMap<SourceId, Vec<Entry>>) -> (Vec<CompareJob>, usize) {
    let mut jobs = groups
        .iter()
        .filter(|group| group_diverges(group.1))
        .filter_map(|(id, entries)| {
            let base = comparator(entries)?;
            if base.rev.is_empty() || !forge_compare::comparable(id) {
                return None; // nothing concrete to compare, or no api to ask
            }
            let mut seen = HashSet::new();
            let heads = entries
                .iter()
                .filter(|entry| {
                    entry.rev != base.rev
                        && !entry.rev.is_empty()
                        && seen.insert(entry.rev.as_str())
                })
                .map(|entry| {
                    CompareJob {
                        id:   id.clone(),
                        base: base.rev.clone(),
                        head: entry.rev.clone(),
                    }
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

/// forge direction of each divergent rev vs its comparator
/// bounded parallel batches keyed by `(group id, full rev)`
/// misses fall back to commit-date ordering
pub(super) fn ahead_behind(
    groups: &BTreeMap<SourceId, Vec<Entry>>,
) -> HashMap<(SourceId, String), CompareStatus> {
    let (jobs, capped) = compare_jobs(groups);
    let attempted = jobs.len();
    let mut compares = HashMap::<(SourceId, String), CompareStatus>::new();
    let mut surfaced = SurfacedCauses::default();
    for chunk in jobs.chunks(MAX_LIVE_COMPARE_JOBS) {
        let batch = chunk
            .into_par_iter()
            .map(|job| (job, forge_compare::compare(&job.id, &job.base, &job.head)))
            .collect::<Vec<_>>();
        for (job, attempt) in batch {
            if let Some(status) = surfaced.record(attempt) {
                compares.insert((job.id.clone(), job.head.clone()), status);
            }
        }
    }

    surfaced.print_tack_messages();
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
mod tests {
    use std::collections::{
        BTreeMap,
        HashMap,
    };

    use super::{
        MAX_COMPARE_JOBS,
        classify,
        comparator,
        compare_jobs,
        group_diverges,
        rev_last_modified,
    };
    use crate::{
        commands::dedup::model::{
            Entry,
            Side,
        },
        fetch::github::CompareStatus,
        report::Mark,
        source::id::SourceId,
    };

    fn entry(path: &[&str], name: &str, rev: &str, lm: Option<u64>) -> Entry {
        entry_full(path, name, rev, rev, lm)
    }

    fn entry_full(
        path: &[&str],
        name: &str,
        _display_rev: &str,
        rev: &str,
        lm: Option<u64>,
    ) -> Entry {
        Entry {
            path: path.iter().map(|item| (*item).to_owned()).collect(),
            name: name.to_owned(),
            side: Side::Flake,
            rev: rev.to_owned(),
            lm,
        }
    }

    fn source_id(str: &str) -> SourceId {
        SourceId::from_url(str).unwrap()
    }

    #[test]
    fn comparator_prefers_top_level_even_without_last_modified() {
        let entries = vec![
            entry(&["parent"], "aaa", "newer", Some(20)),
            entry(&[], "top", "top-rev", None),
        ];
        assert_eq!(
            comparator(&entries).map(|entry| (entry.rev.as_str(), entry.lm)),
            Some(("top-rev", None))
        );
    }

    #[test]
    fn comparator_uses_newest_known_transitive_then_deterministic_fallback() {
        let entries_with_known_time = vec![
            entry(&["parent"], "aaa", "unknown", None),
            entry(&["parent"], "bbb", "older", Some(10)),
            entry(&["parent"], "ccc", "newer", Some(20)),
        ];
        assert_eq!(
            comparator(&entries_with_known_time).map(|entry| (entry.rev.as_str(), entry.lm)),
            Some(("newer", Some(20)))
        );

        let entries_without_times = vec![
            entry(&["parent"], "bbb", "unknown-b", None),
            entry(&["parent"], "aaa", "unknown-a", None),
        ];
        assert_eq!(
            comparator(&entries_without_times).map(|entry| (entry.rev.as_str(), entry.lm)),
            Some(("unknown-a", None))
        );
    }

    #[test]
    fn group_divergence_uses_semantic_revs() {
        let entries = vec![
            entry_full(
                &[],
                "base",
                "abcdef0",
                "abcdef0000000000000000000000000000000000",
                Some(10),
            ),
            entry_full(
                &["dep"],
                "head",
                "abcdef0",
                "abcdef0999999999999999999999999999999999",
                Some(20),
            ),
        ];

        assert!(group_diverges(&entries));
    }

    #[test]
    fn compare_jobs_use_semantic_revs() {
        let mut groups = BTreeMap::new();
        groups.insert(source_id("github:o/r"), vec![
            entry_full(
                &[],
                "base",
                "1111111",
                "1111111111111111111111111111111111111111",
                Some(10),
            ),
            entry_full(
                &["dep"],
                "head",
                "2222222",
                "2222222222222222222222222222222222222222",
                Some(20),
            ),
        ]);

        let (jobs, capped) = compare_jobs(&groups);

        assert_eq!(capped, 0);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].base, "1111111111111111111111111111111111111111");
        assert_eq!(jobs[0].head, "2222222222222222222222222222222222222222");
    }

    #[test]
    fn compare_jobs_cover_gitlab_and_plain_git() {
        let mut groups = BTreeMap::new();
        groups.insert(source_id("gitlab:o/r"), vec![
            entry(&[], "base", "base", Some(10)),
            entry(&["dep"], "head", "head", Some(20)),
        ]);
        groups.insert(source_id("git+https://example.com/o/r.git"), vec![
            entry(&[], "base", "base", Some(10)),
            entry(&["dep"], "head", "head", Some(20)),
        ]);

        let (jobs, capped) = compare_jobs(&groups);

        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].id, source_id("git+https://example.com/o/r.git"));
        assert_eq!(jobs[1].id, source_id("gitlab:o/r"));
        assert_eq!(capped, 0);
    }

    #[test]
    fn compare_jobs_are_capped_before_network_work() {
        let mut entries = vec![entry_full(&[], "base", "base", "base-full", Some(0))];
        for i in 0..(MAX_COMPARE_JOBS + 5) {
            entries.push(entry_full(
                &["dep"],
                &format!("head-{i:03}"),
                &format!("h{i:06}"),
                &format!("head-full-{i:03}"),
                Some(u64::try_from(i).unwrap() + 1),
            ));
        }
        let mut groups = BTreeMap::new();
        groups.insert(source_id("github:o/r"), entries);

        let (jobs, capped) = compare_jobs(&groups);

        assert_eq!(jobs.len(), MAX_COMPARE_JOBS);
        assert_eq!(capped, 5);
    }

    #[test]
    fn classify_prefers_branch_status_over_misleading_timestamps() {
        let id = source_id("github:o/r");
        let entries = vec![
            entry(&[], "base", "base", Some(500)),
            entry(&["dep"], "head", "head", Some(100)),
        ];
        let compares = HashMap::from([((id.clone(), "head".to_owned()), CompareStatus::Ahead)]);

        let mark = classify(
            &id,
            "head",
            comparator(&entries),
            &rev_last_modified(&entries),
            &compares,
        );

        assert_eq!(mark, Mark::Ahead);
    }

    #[test]
    fn classify_reports_diverged_branch_status() {
        let id = source_id("github:o/r");
        let entries = vec![
            entry(&[], "base", "base", Some(100)),
            entry(&["dep"], "head", "head", Some(200)),
        ];
        let compares = HashMap::from([((id.clone(), "head".to_owned()), CompareStatus::Diverged)]);

        let mark = classify(
            &id,
            "head",
            comparator(&entries),
            &rev_last_modified(&entries),
            &compares,
        );

        assert_eq!(mark, Mark::Diverged);
    }

    #[test]
    fn classify_distinguishes_timestamp_fallback() {
        let id = source_id("github:o/r");
        let entries = vec![
            entry(&[], "base", "base", Some(100)),
            entry(&["dep"], "head", "head", Some(200)),
        ];
        let compares = HashMap::<(SourceId, String), CompareStatus>::new();

        let mark = classify(
            &id,
            "head",
            comparator(&entries),
            &rev_last_modified(&entries),
            &compares,
        );

        assert_eq!(mark, Mark::DatedNewer);
    }
}
