// SPDX-License-Identifier: EUPL-1.2

use rayon::prelude::{
    IntoParallelIterator as _,
    ParallelIterator as _,
};

use super::{
    BTreeMap,
    BTreeSet,
    CompareStatus,
    Forge,
    HashMap,
    HashSet,
    Path,
    PinType,
    Project,
    Result,
    Source,
    SourceId,
    Value,
    cmp,
    fetch,
    fs,
    lock,
    mem,
    pins,
    render,
    shorturl,
    tolerate,
    top_map,
};

/// which side of an upstream a finding came from. a `flake:`/`tack:`-scoped
/// follow only matches its own side, though a bare follow matches both.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::commands) enum Side {
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

pub(in crate::commands) struct Entry {
    /// lineage from top-pin down to the parent tree being scanned
    pub path:     Vec<String>,
    pub name:     String,
    /// flake input vs upstream tack pin, for side-scoped follow matching
    pub side:     Side,
    /// abbreviated rev
    pub rev:      String,
    /// untruncated rev
    pub full_rev: String,
    /// `lastModified` of the locked node
    pub lm:       Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::commands) struct CompareJob {
    pub id:    SourceId,
    pub owner: String,
    pub repo:  String,
    pub base:  String,
    pub head:  String,
}

/// The render-agnostic result of a dedup scan: only the diverging groups plus
/// the follow suggestions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DedupReport {
    pub groups:      Vec<DedupGroup>,
    /// alias -> target for groups that have a top-level pin
    pub pin_follow:  BTreeMap<String, String>,
    /// alias -> target for transitive-only groups
    pub auto_follow: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DedupGroup {
    pub id:    String,
    pub count: usize,
    pub revs:  Vec<RevGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevGroup {
    pub rev:   String,
    pub mark:  Mark,
    pub names: Vec<NameSources>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameSources {
    pub name:    String,
    pub sources: Vec<String>,
}

/// A rev's position relative to its group comparator, decided by the data
/// layer. Rendering maps this to glyphs, colors, and widths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Base,
    Ahead,
    Behind,
    Diverged,
    DatedNewer,
    DatedOlder,
    DatedEqual,
    Unknown,
}

struct Finding {
    identity: SourceId,
    entry:    Entry,
}

struct ScanResult {
    findings:   Vec<Finding>,
    transitive: Vec<TackTransitive>,
}

struct TackTransitive {
    path:       Vec<String>,
    source:     SourceRef,
    submodules: bool,
}

enum SourceRef {
    Locked(Value),
    Url(String),
}

type RawProbeSet = (Option<String>, Option<String>, Option<String>);
type RawProbeOutcome = (Option<RawProbeSet>, BTreeSet<String>);

pub(in crate::commands) const MAX_COMPARE_JOBS: usize = 100;
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
    // a bare follow reaches both sides, a `flake:`/`tack:` follow only its own
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

/// align each followed entry onto its target, which provides rev, full rev and
/// `lastModified`.
pub(in crate::commands) fn apply_follows(
    groups: &mut BTreeMap<SourceId, Vec<Entry>>,
    by_name: &BTreeMap<&str, &pins::Input>,
    all_follow: &BTreeMap<String, String>,
    top_revs: &BTreeMap<String, String>,
    top_full_revs: &BTreeMap<String, String>,
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
        // `follow_target` only returns targets present in `top_revs`, and
        // `top_full_revs` shares its key set
        if let Some(rev) = top_revs.get(&target) {
            entry.rev.clone_from(rev);
        }
        if let Some(full_rev) = top_full_revs.get(&target) {
            entry.full_rev.clone_from(full_rev);
        }
        entry.lm = top_lms.get(&target).copied();
    }
}

pub fn dedup() -> Result<()> {
    let project = Project::discover();
    let doc = project.load_pins()?;
    let lock = project.load_lock()?;
    let inputs = pins::inputs(&doc)?;
    let shorturls = pins::shorturls(&doc);
    let all_follow = pins::all_follows(&doc);
    let by_name = inputs
        .iter()
        .map(|inp| (inp.name.as_str(), inp))
        .collect::<BTreeMap<&str, &pins::Input>>();

    let top_revs = top_map(&inputs, &lock, |n| {
        lock::Node::from(n).full_rev().map(render::short)
    });
    let top_full_revs = top_map(&inputs, &lock, |n| {
        lock::Node::from(n).full_rev().map(str::to_owned)
    });
    let top_lms = top_map(&inputs, &lock, |n| lock::Node::from(n).last_modified());

    let mut groups = BTreeMap::<SourceId, Vec<Entry>>::new();

    for inp in &inputs {
        let expanded = shorturl::expand(&inp.url, &shorturls);
        if let Some(id) = SourceId::from_url(&expanded) {
            let rev = top_revs.get(&inp.name).cloned().unwrap_or_default();
            let full_rev = top_full_revs.get(&inp.name).cloned().unwrap_or_default();
            let lm = lock
                .get(&inp.name)
                .and_then(|n| lock::Node::from(n).last_modified());
            groups.entry(id).or_default().push(Entry {
                path: vec![],
                name: inp.name.clone(),
                side: Side::Flake,
                rev,
                full_rev,
                lm,
            });
        }
    }

    let mut frontier = inputs
        .iter()
        .filter_map(|inp| {
            let node = lock.get(&inp.name)?;
            (inp.pin_type == PinType::Flake).then(|| {
                TackTransitive {
                    path:       vec![inp.name.clone()],
                    source:     SourceRef::Locked(node.clone()),
                    submodules: inp.submodules,
                }
            })
        })
        .collect::<Vec<TackTransitive>>();
    eprintln!("scanning {} pin(s)...", frontier.len());

    // bfs level-by-level: dedup the frontier against `visited`, fetch the
    // batch in parallel, then expand into the next frontier
    let mut visited = HashSet::<String>::new();
    while !frontier.is_empty() {
        let results = mem::take(&mut frontier)
            .into_iter()
            .filter(|item| visited.insert(source_key(&item.source)))
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|item| (item.path.clone(), fetch_and_scan(&item)))
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

    apply_follows(
        &mut groups,
        &by_name,
        &all_follow,
        &top_revs,
        &top_full_revs,
        &top_lms,
    );

    let compares = ahead_behind(&groups);
    let report = build_report(&groups, &all_follow, &compares);
    render::print_report(&report);
    Ok(())
}

fn fetch_and_scan(item: &TackTransitive) -> Result<ScanResult> {
    // fetch only the 3 files scan needs via http if the source's forge supports it
    if let SourceRef::Locked(ref node) = item.source
        && let (Some((flake_lock, tack_pins, tack_lock)), surfaced) = try_raw_files(node)
    {
        for cause in &surfaced {
            eprintln!("tack: {cause}");
        }
        return Ok(scan_files(
            flake_lock.as_deref(),
            tack_pins.as_deref(),
            tack_lock.as_deref(),
            &item.path,
        ));
    }

    let tmp = tempfile::tempdir()?;
    let root = match item.source {
        SourceRef::Locked(ref node) => fetch::fetch_locked_tree_into(node, tmp.path())?,
        SourceRef::Url(ref url) => {
            let source = url.parse::<Source>()?;
            fetch::fetch_tree_into(&source, item.submodules, tmp.path())?
        },
    };
    Ok(scan_tree(&root, &item.path))
}

/// `authoritative = true` means a 404 on every probe is final, and `false`
/// means the caller should fall back to clone
fn try_raw_files(node: &Value) -> RawProbeOutcome {
    let Some(forge) = Forge::from_node(node) else {
        return (None, BTreeSet::new());
    };
    let Some(rev) = lock::Node::from(node).rev() else {
        return (None, BTreeSet::new());
    };
    let mut surfaced = BTreeSet::new();
    let mut probe = |file| {
        let (value, maybe_cause) = tolerate(fetch_forge_file(&forge, rev, file));
        if let Some(cause) = maybe_cause {
            surfaced.insert(cause);
        }
        value
    };
    let triple = (
        probe("flake.lock"),
        probe(".tack/pins.toml"),
        probe(".tack/pins.lock.json"),
    );
    if !forge.authoritative() && triple.0.is_none() && triple.1.is_none() && triple.2.is_none() {
        (None, surfaced)
    } else {
        (Some(triple), surfaced)
    }
}

fn fetch_forge_file(forge: &Forge, rev: &str, file: &str) -> Result<String, fetch::FetchError> {
    let raw = forge.raw_file_url(rev, file);
    let body = fetch::raw(&raw.url)?;
    match raw.decoder {
        Some(decode) => {
            decode(&body).map_err(|source| {
                fetch::FetchError::Decode {
                    what: file.to_owned(),
                    source,
                }
            })
        },
        None => Ok(body),
    }
}

/// Fetch one file from a locked node via raw http. Unknown hosts or missing
/// revs simply ask the caller to skip the raw path.
pub(in crate::commands) fn try_raw_file(
    node: &Value,
    file: &str,
) -> Result<Option<String>, fetch::FetchError> {
    let Some(forge) = Forge::from_node(node) else {
        return Ok(None);
    };
    let Some(rev) = lock::Node::from(node).rev() else {
        return Ok(None);
    };
    fetch_forge_file(&forge, rev, file).map(Some)
}

fn scan_tree(root: &Path, path: &[String]) -> ScanResult {
    let flake_lock = fs::read_to_string(root.join("flake.lock")).ok();
    let td = root.join(".tack");
    let tack_pins = fs::read_to_string(td.join("pins.toml")).ok();
    let tack_lock = fs::read_to_string(td.join("pins.lock.json")).ok();
    scan_files(
        flake_lock.as_deref(),
        tack_pins.as_deref(),
        tack_lock.as_deref(),
        path,
    )
}

fn scan_files(
    flake_lock: Option<&str>,
    tack_pins: Option<&str>,
    tack_lock: Option<&str>,
    path: &[String],
) -> ScanResult {
    let mut findings = Vec::<Finding>::new();
    let mut transitive = Vec::<TackTransitive>::new();

    if let Some(raw) = flake_lock
        && let Ok(json) = serde_json::from_str::<Value>(raw)
    {
        let root_key = json.get("root").and_then(Value::as_str).unwrap_or("root");
        if let Some(nodes) = json.get("nodes").and_then(Value::as_object) {
            for (key, node) in nodes {
                if key == root_key {
                    continue;
                }
                let Some(locked) = node.get("locked") else {
                    continue;
                };
                if let Some(id) = SourceId::from_node(locked) {
                    findings.push(Finding {
                        identity: id,
                        entry:    Entry {
                            path:     path.to_vec(),
                            name:     strip_disambiguator(key).to_owned(),
                            side:     Side::Flake,
                            rev:      lock::Node::from(locked)
                                .full_rev()
                                .map(render::short)
                                .unwrap_or_default(),
                            full_rev: lock::Node::from(locked)
                                .full_rev()
                                .map(str::to_owned)
                                .unwrap_or_default(),
                            lm:       lock::Node::from(locked).last_modified(),
                        },
                    });
                }
            }
        }
    }

    if let Some(raw) = tack_pins
        && let Ok(doc) = pins::parse_doc(raw)
        && let Ok(tinputs) = pins::inputs(&doc)
    {
        let tlock = tack_lock
            .and_then(|str| lock::parse(str).ok())
            .unwrap_or_default();
        let tshort = pins::shorturls(&doc);
        for tinp in &tinputs {
            let expanded = shorturl::expand(&tinp.url, &tshort);
            if let Some(id) = SourceId::from_url(&expanded) {
                findings.push(Finding {
                    identity: id,
                    entry:    Entry {
                        path:     path.to_vec(),
                        name:     tinp.name.clone(),
                        side:     Side::Tack,
                        rev:      tlock
                            .get(&tinp.name)
                            .and_then(|n| lock::Node::from(n).full_rev().map(render::short))
                            .unwrap_or_default(),
                        full_rev: tlock
                            .get(&tinp.name)
                            .and_then(|n| lock::Node::from(n).full_rev().map(str::to_owned))
                            .unwrap_or_default(),
                        lm:       tlock
                            .get(&tinp.name)
                            .and_then(|n| lock::Node::from(n).last_modified()),
                    },
                });
            }
            if tinp.pin_type != PinType::Fixed {
                let mut next = path.to_vec();
                next.push(tinp.name.clone());
                let source = tlock
                    .get(&tinp.name)
                    .map_or(SourceRef::Url(expanded), |node| {
                        SourceRef::Locked(node.clone())
                    });
                transitive.push(TackTransitive {
                    path: next,
                    source,
                    submodules: tinp.submodules,
                });
            }
        }
    }

    ScanResult {
        findings,
        transitive,
    }
}

fn source_key(source: &SourceRef) -> String {
    match *source {
        SourceRef::Locked(ref node) => {
            SourceId::from_node(node).map_or_else(|| node.to_string(), |id| id.to_string())
        },
        SourceRef::Url(ref url) => url.clone(),
    }
}

/// flake.lock disambiguates same-named nodes as `name_2`, `name_3`, ...;
/// recover the original input name so dedup groups by what the parent flake
/// actually declares
pub(in crate::commands) fn strip_disambiguator(key: &str) -> &str {
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

/// the entry a group is measured against. returns the version pinned at top,
/// else the newest transitive version by `lastModified`, else the lowest-named
/// entry for a deterministic fallback
pub(in crate::commands) fn comparator(entries: &[Entry]) -> Option<&Entry> {
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
pub(in crate::commands) fn group_diverges(entries: &[Entry]) -> bool {
    let mut revs = entries.iter().map(entry_compare_rev);
    revs.next()
        .is_some_and(|first| revs.any(|rev| rev != first))
}

const fn entry_compare_rev(entry: &Entry) -> &str {
    if entry.full_rev.is_empty() {
        entry.rev.as_str()
    } else {
        entry.full_rev.as_str()
    }
}

/// build github compare work for every divergent rev against its comparator.
/// returned jobs carry full revs for both the network request and the result
/// map keys; rendering remains separately abbreviated.
pub(in crate::commands) fn compare_jobs(
    groups: &BTreeMap<SourceId, Vec<Entry>>,
) -> (Vec<CompareJob>, usize) {
    let mut jobs = groups
        .iter()
        .filter(|group| group_diverges(group.1))
        .filter_map(|(id, entries)| {
            let base = comparator(entries)?;
            if base.full_rev.is_empty() {
                return None; // nothing concrete to compare against
            }
            let (owner, repo) = id.github_parts()?;
            let mut seen = HashSet::new();
            let heads = entries
                .iter()
                .filter(|entry| {
                    entry.full_rev != base.full_rev
                        && !entry.full_rev.is_empty()
                        && seen.insert(entry.full_rev.as_str())
                })
                .map(|entry| {
                    CompareJob {
                        id:    id.clone(),
                        owner: owner.to_owned(),
                        repo:  repo.to_owned(),
                        base:  base.full_rev.clone(),
                        head:  entry.full_rev.clone(),
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

/// ask github for the direction of every divergent rev against its comparator,
/// in bounded parallel batches. keyed by `(group id, full rev)`. misses are
/// reported and fall back to commit-date ordering.
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
                let (maybe_status, surfaced_cause) = tolerate(fetch::compare_status(
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

pub(in crate::commands) fn rev_last_modified(entries: &[Entry]) -> BTreeMap<&str, u64> {
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

pub(in crate::commands) fn classify(
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

type SourcesByRev<'a> = BTreeMap<&'a str, (&'a str, BTreeMap<&'a str, Vec<String>>)>;

fn group_sources_by_rev(entries: &[Entry]) -> SourcesByRev<'_> {
    let mut by_rev = BTreeMap::<&str, (&str, BTreeMap<&str, Vec<String>>)>::new();
    for entry in entries {
        let names = &mut by_rev
            .entry(entry_compare_rev(entry))
            .or_insert_with(|| (entry.rev.as_str(), BTreeMap::new()))
            .1;
        names
            .entry(entry.name.as_str())
            .or_default()
            .push(render::source_label(&entry.path));
    }
    by_rev
}

fn build_report(
    groups: &BTreeMap<SourceId, Vec<Entry>>,
    all_follow: &BTreeMap<String, String>,
    compares: &HashMap<(SourceId, String), CompareStatus>,
) -> DedupReport {
    // alias -> target, paste-ready under [all_follow]
    let mut pin_follow = BTreeMap::<String, String>::new();
    let mut auto_follow = BTreeMap::<String, String>::new();
    let mut report_groups = Vec::<DedupGroup>::new();

    for (id, entries) in groups {
        // single source, or already aligned by follows: nothing to show
        if !group_diverges(entries) {
            continue;
        }

        let by_rev = group_sources_by_rev(entries);
        let comp = comparator(entries);
        let lm_of = rev_last_modified(entries);
        let revs = by_rev
            .into_iter()
            .map(|(rev, (display_rev, name_map))| {
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
                    rev:   display_rev.to_owned(),
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
                    pin_follow.insert(entry.name.clone(), top.to_owned());
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
                    auto_follow.insert(alias.clone(), canonical.clone());
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
        pin_follow,
        auto_follow,
    }
}

/// suggested top-level name for a transitive-only group. this uses the github
/// repo basename when available, else the shortest alias seen
pub(in crate::commands) fn pick_name(id: &SourceId, aliases: &BTreeSet<String>) -> String {
    if let Some((_, repo)) = id.github_parts() {
        return repo.trim_end_matches(".nix").replace('.', "-");
    }
    aliases
        .iter()
        .min_by_key(|name| (name.len(), name.as_str()))
        .cloned()
        .unwrap_or_default()
}
