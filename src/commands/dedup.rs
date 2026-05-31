// SPDX-License-Identifier: EUPL-1.2

use std::{
    cmp,
    collections::{
        BTreeMap,
        BTreeSet,
        HashMap,
        HashSet,
    },
    fs,
    mem,
    path::{
        Path,
        PathBuf,
    },
};

use eyre::Result;
use rayon::prelude::{
    IntoParallelIterator as _,
    ParallelIterator as _,
};

use super::{
    tolerate,
    top_map,
};
use crate::{
    fetch::{
        self,
        github::CompareStatus,
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
    project::Project,
    render,
    report::{
        DedupGroup,
        DedupReport,
        FollowSuggestions,
        Mark,
        NameSources,
        RevGroup,
    },
    source::{
        Source,
        forge::Forge,
        id::SourceId,
    },
};

/// which side of an upstream a finding came from
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    Flake,
    Tack,
}

impl Side {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Flake => "flake",
            Self::Tack => "tack",
        }
    }
}

pub(super) struct Entry {
    /// lineage from top-pin down to the parent tree being scanned
    pub path: Vec<String>,
    pub name: String,
    /// flake input vs upstream tack pin, for side-scoped follow matching
    pub side: Side,
    /// untruncated rev
    pub rev:  String,
    /// `lastModified` of the locked node
    pub lm:   Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CompareJob {
    pub id:    SourceId,
    pub owner: String,
    pub repo:  String,
    pub base:  String,
    pub head:  String,
}

struct Finding {
    identity: SourceId,
    entry:    Entry,
}

struct ScanResult {
    findings:   Vec<Finding>,
    transitive: Vec<ScanTarget>,
}

struct ScanTarget {
    path:       Vec<String>,
    source:     SourceRef,
    submodules: bool,
}

impl ScanTarget {
    fn fetch_and_scan(&self) -> Result<ScanResult> {
        if let SourceRef::Locked(ref node) = self.source
            && let Some(documents) = RawProbe::documents(node).into_documents()
        {
            return Ok(documents.scan(&self.path));
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

enum SourceRef {
    Locked(LockedNode),
    Url(String),
}

impl SourceRef {
    fn key(&self) -> String {
        match *self {
            Self::Locked(ref node) => {
                SourceId::from_locked(node).map_or_else(
                    || format!("{}:{}", node.kind(), node.full_rev().unwrap_or("")),
                    |id| id.to_string(),
                )
            },
            Self::Url(ref url) => url.clone(),
        }
    }
}

struct ScanDocuments {
    flake_lock: Option<String>,
    tack_pins:  Option<String>,
    tack_lock:  Option<String>,
}

struct RawProbeOutcome {
    documents: Option<ScanDocuments>,
    surfaced:  BTreeSet<String>,
}

impl RawProbeOutcome {
    const fn empty() -> Self {
        Self {
            documents: None,
            surfaced:  BTreeSet::new(),
        }
    }

    fn into_documents(self) -> Option<ScanDocuments> {
        let documents = self.documents?;
        for cause in self.surfaced {
            eprintln!("tack: {cause}");
        }
        Some(documents)
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
    fn documents(node: &'a LockedNode) -> RawProbeOutcome {
        let Some(probe) = Self::from_locked(node) else {
            return RawProbeOutcome::empty();
        };
        probe.probe_documents()
    }

    fn probe_documents(&self) -> RawProbeOutcome {
        let mut surfaced = BTreeSet::new();
        let mut probe = |file| {
            let (value, maybe_cause) = tolerate(self.fetch(file));
            if let Some(cause) = maybe_cause {
                surfaced.insert(cause);
            }
            value
        };
        let documents = ScanDocuments {
            flake_lock: probe("flake.lock"),
            tack_pins:  probe(".tack/pins.toml"),
            tack_lock:  probe(".tack/pins.lock.json"),
        };
        if !self.forge.authoritative()
            && documents.flake_lock.is_none()
            && documents.tack_pins.is_none()
            && documents.tack_lock.is_none()
        {
            RawProbeOutcome {
                documents: None,
                surfaced,
            }
        } else {
            RawProbeOutcome {
                documents: Some(documents),
                surfaced,
            }
        }
    }

    fn fetch(&self, file: &str) -> Result<String, FetchError> {
        let raw = self.forge.raw_file_url(self.rev, file);
        let body = fetch::raw(&raw.url)?;
        match raw.decoder {
            Some(decode) => {
                decode(&body).map_err(|source| {
                    FetchError::Decode {
                        what: file.to_owned(),
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

        self.scan_flake_lock(path, &mut findings);
        self.scan_tack_inputs(path, &mut findings, &mut transitive);

        ScanResult {
            findings,
            transitive,
        }
    }

    fn scan_flake_lock(&self, path: &[String], findings: &mut Vec<Finding>) {
        if let Some(raw) = self.flake_lock.as_deref()
            && let Ok(doc) = lock::FlakeLock::parse(raw)
        {
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
    }

    fn scan_tack_inputs(
        &self,
        path: &[String],
        findings: &mut Vec<Finding>,
        transitive: &mut Vec<ScanTarget>,
    ) {
        if let Some(raw) = self.tack_pins.as_deref()
            && let Ok(doc) = pins::PinsDoc::parse(raw)
            && let Ok(tinputs) = doc.inputs()
        {
            let tlock = self
                .tack_lock
                .as_deref()
                .and_then(|str| lock::parse(str).ok())
                .unwrap_or_default();
            let tshort = doc.shorturls();
            for tinp in &tinputs {
                let expanded = tshort.expand(&tinp.url);
                Self::record_tack_finding(path, tinp, &expanded, &tlock, findings);
                Self::queue_tack_transitive(path, tinp, expanded, &tlock, transitive);
            }
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

pub(super) const MAX_COMPARE_JOBS: usize = 100;
const MAX_LIVE_COMPARE_JOBS: usize = 8;

fn follow_target(
    path: &[String],
    name: &str,
    side: Side,
    top_input: Option<&pins::Input>,
    all_follow: &BTreeMap<String, String>,
    top_revs: &BTreeMap<String, String>,
) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    // a bare follow reaches both sides, but a `flake:`/`tack:` follow only its own
    let scoped = format!("{}:{name}", side.as_str());
    let excluded = top_input.is_some_and(|inp| inp.excludes.contains(name));
    if !excluded
        && let Some(target) = all_follow.get(name).or_else(|| all_follow.get(&scoped))
        && top_revs.contains_key(target)
    {
        return Some(target.clone());
    }
    if path.len() == 1
        && let Some(inp) = top_input
        && let Some(target) = inp.follows.get(name).or_else(|| inp.follows.get(&scoped))
        && top_revs.contains_key(target)
    {
        return Some(target.clone());
    }
    None
}

/// align each followed entry onto its target
pub(super) fn apply_follows(
    groups: &mut BTreeMap<SourceId, Vec<Entry>>,
    by_name: &BTreeMap<&str, &pins::Input>,
    all_follow: &BTreeMap<String, String>,
    top_revs: &BTreeMap<String, String>,
    top_lms: &BTreeMap<String, u64>,
) {
    for entry in groups.values_mut().flatten() {
        let top = entry
            .path
            .first()
            .and_then(|name| by_name.get(name.as_str()).copied());
        let Some(target) = follow_target(
            &entry.path,
            &entry.name,
            entry.side,
            top,
            all_follow,
            top_revs,
        ) else {
            continue;
        };
        if let Some(rev) = top_revs.get(&target) {
            entry.rev.clone_from(rev);
        }
        entry.lm = top_lms.get(&target).copied();
    }
}

pub fn dedup(project: &Project) -> Result<()> {
    let doc = project.load_pins()?;
    let lock = project.load_lock()?;
    let inputs = doc.inputs()?;
    let shorturls = doc.shorturls();
    let all_follow = doc.all_follows()?;
    let by_name = inputs
        .iter()
        .map(|inp| (inp.name.as_str(), inp))
        .collect::<BTreeMap<&str, &pins::Input>>();

    let top_revs = top_map(&inputs, &lock, |n| n.full_rev().map(str::to_owned));
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
    eprintln!("scanning {} pin(s)...", frontier.len());

    // bfs level-by-level: dedup the frontier against `visited`, fetch the
    // batch in parallel, then expand into the next frontier
    let mut visited = HashSet::<String>::new();
    while !frontier.is_empty() {
        let results = mem::take(&mut frontier)
            .into_iter()
            .filter(|item| visited.insert(item.source.key()))
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|item| (item.path.clone(), item.fetch_and_scan()))
            .collect::<Vec<_>>();

        for (path, res) in results {
            match res {
                Ok(scan) => {
                    for finding in scan.findings {
                        groups
                            .entry(finding.identity)
                            .or_default()
                            .push(finding.entry);
                    }
                    frontier.extend(scan.transitive);
                },
                Err(err) => eprintln!("tack: scan {}: {err:#}", path.join(" > ")),
            }
        }
    }

    apply_follows(&mut groups, &by_name, &all_follow, &top_revs, &top_lms);

    let compares = ahead_behind(&groups);
    let report = build_report(&groups, &all_follow, &compares);
    render::print_report(&report);
    Ok(())
}

/// fetch one file from a locked node via raw http
/// unknown hosts or missing revs tell the caller to skip the raw path
pub(super) fn try_raw_file(node: &LockedNode, file: &str) -> Result<Option<String>, FetchError> {
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

/// the entry a group is measured against
/// returns the version pinned at top, else the newest transitive version by
/// `lastModified`, else the lowest-named entry for a deterministic fallback
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

/// a group is worth printing only when its revs disagree
pub(super) fn group_diverges(entries: &[Entry]) -> bool {
    let mut revs = entries.iter().map(entry_compare_rev);
    revs.next()
        .is_some_and(|first| revs.any(|rev| rev != first))
}

const fn entry_compare_rev(entry: &Entry) -> &str {
    entry.rev.as_str()
}

/// build github compare work for every divergent rev against its comparator
/// returned jobs carry full revs for both the network request and the result
/// map keys, rendering remains separately abbreviated
pub(super) fn compare_jobs(groups: &BTreeMap<SourceId, Vec<Entry>>) -> (Vec<CompareJob>, usize) {
    let mut jobs = groups
        .iter()
        .filter(|group| group_diverges(group.1))
        .filter_map(|(id, entries)| {
            let base = comparator(entries)?;
            if base.rev.is_empty() {
                return None; // nothing concrete to compare against
            }
            let (owner, repo) = id.github_parts()?;
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
                        id:    id.clone(),
                        owner: owner.to_owned(),
                        repo:  repo.to_owned(),
                        base:  base.rev.clone(),
                        head:  entry.rev.clone(),
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

/// ask github for the direction of every divergent rev against its comparator
/// runs in bounded parallel batches keyed by `(group id, full rev)`
/// misses are reported and fall back to commit-date ordering
fn ahead_behind(
    groups: &BTreeMap<SourceId, Vec<Entry>>,
) -> HashMap<(SourceId, String), CompareStatus> {
    let (jobs, capped) = compare_jobs(groups);
    let attempted = jobs.len();
    let mut compares = HashMap::<(SourceId, String), CompareStatus>::new();
    let mut surfaced = BTreeSet::<String>::new();
    for chunk in jobs.chunks(MAX_LIVE_COMPARE_JOBS) {
        let batch = chunk
            .into_par_iter()
            .map(|job| {
                let (maybe_status, surfaced_cause) = tolerate(fetch::github::compare_status(
                    &job.owner, &job.repo, &job.base, &job.head,
                ));
                (
                    maybe_status.flatten().map(|comparison_status| {
                        ((job.id.clone(), job.head.clone()), comparison_status)
                    }),
                    surfaced_cause,
                )
            })
            .collect::<Vec<_>>();
        for (comparison, maybe_cause) in batch {
            if let Some((key, status)) = comparison {
                compares.insert(key, status);
            }
            if let Some(cause) = maybe_cause {
                surfaced.insert(cause);
            }
        }
    }

    for cause in &surfaced {
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

type SourcesByRev<'a> = BTreeMap<&'a str, BTreeMap<&'a str, Vec<Vec<String>>>>;

fn group_sources_by_rev(entries: &[Entry]) -> SourcesByRev<'_> {
    let mut by_rev = BTreeMap::<&str, BTreeMap<&str, Vec<Vec<String>>>>::new();
    for entry in entries {
        let names = &mut by_rev.entry(entry_compare_rev(entry)).or_default();
        names
            .entry(entry.name.as_str())
            .or_default()
            .push(entry.path.clone());
    }
    by_rev
}

fn build_report(
    groups: &BTreeMap<SourceId, Vec<Entry>>,
    all_follow: &BTreeMap<String, String>,
    compares: &HashMap<(SourceId, String), CompareStatus>,
) -> DedupReport {
    let mut follows = FollowSuggestions::default();
    let mut report_groups = Vec::<DedupGroup>::new();

    for (id, entries) in groups {
        // single source, or already aligned by follows, means nothing to show
        if !group_diverges(entries) {
            continue;
        }

        let by_rev = group_sources_by_rev(entries);
        let comp = comparator(entries);
        let lm_of = rev_last_modified(entries);
        let revs = by_rev
            .into_iter()
            .map(|(rev, name_map)| {
                let name_sources = name_map
                    .into_iter()
                    .map(|(name, sources)| {
                        NameSources {
                            name: name.to_owned(),
                            sources,
                        }
                    })
                    .collect::<Vec<_>>();
                RevGroup {
                    rev:   rev.to_owned(),
                    mark:  classify(id, rev, comp, &lm_of, compares),
                    names: name_sources,
                }
            })
            .collect::<Vec<_>>();

        let top_name = entries
            .iter()
            .filter(|entry| entry.path.is_empty())
            .map(|entry| entry.name.as_str())
            .min();
        if let Some(top) = top_name {
            for entry in entries {
                if !entry.path.is_empty() && !all_follow.contains_key(&entry.name) {
                    follows.pin.insert(entry.name.clone(), top.to_owned());
                }
            }
        } else {
            let aliases = entries
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<BTreeSet<String>>();
            let canonical = pick_name(id, &aliases);
            for alias in &aliases {
                if !all_follow.contains_key(alias) {
                    follows.auto.insert(alias.clone(), canonical.clone());
                }
            }
        }

        report_groups.push(DedupGroup {
            id: id.to_string(),
            count: entries.len(),
            revs,
        });
    }

    DedupReport {
        groups: report_groups,
        follows,
    }
}

/// suggested top-level name for a transitive-only group
/// uses the github repo basename when available, else the shortest alias seen
pub(super) fn pick_name(id: &SourceId, aliases: &BTreeSet<String>) -> String {
    if let Some((_, repo)) = id.github_parts() {
        return repo.trim_end_matches(".nix").replace('.', "-");
    }
    aliases
        .iter()
        .min_by_key(|name| (name.len(), name.as_str()))
        .cloned()
        .unwrap_or_default()
}
