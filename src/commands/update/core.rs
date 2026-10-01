// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    collections::BTreeSet,
    mem,
};

use misstep::Result;

use super::LOG_LIMIT;
use crate::{
    commands::{
        Selection,
        dedup,
        select,
    },
    dispatcher,
    fetch::{
        self,
        BranchComparison,
        CommitLog,
        CompareStatus,
        FetchedPin,
        FetchedTree,
        compare_planner::{
            CompareJob,
            CompareSession,
        },
    },
    lock::{
        LockFile,
        LockIdentity,
        LockedNode,
        SignedBy,
    },
    patched::{
        self,
        Mode,
        PatchedPin,
        Settled,
    },
    pins::{
        self,
        PinType,
        Unpack,
    },
    project::Project,
    render,
    report::{
        Dates,
        LookOutcome,
        LookReport,
        PinLook,
        PinUpdate,
        Signed,
        UpdateOutcome,
        UpdateReport,
    },
    signers::{
        Anchor,
        Keyring,
        Verdict,
    },
    source::{
        self,
        Source,
        id::SourceId,
    },
    tag::{
        self,
        Followed,
    },
};

const UPDATE_IN_FLIGHT: usize = 16;
const LOOK_IN_FLIGHT: usize = 16;

pub(super) trait Progress<O>: Sync {
    fn begin(&self, selected: &[&pins::Input]);

    fn fetching(&self, index: usize);

    fn finished(&self, index: usize, outcome: &O);
}

pub(super) struct NoProgress;

impl<O> Progress<O> for NoProgress {
    fn begin(&self, _selected: &[&pins::Input]) {}

    fn fetching(&self, _index: usize) {}

    fn finished(&self, _index: usize, _outcome: &O) {}
}

pub fn fetch_input(
    pin_type: PinType,
    unpack: Option<Unpack>,
    submodules: bool,
    expanded: &str,
) -> Result<FetchedPin> {
    match pin_type {
        PinType::Fixed => fetch::fetch_fixed_pin(expanded, unpack),
        PinType::Flake | PinType::Fetch => {
            let source = expanded.parse::<Source>()?;
            fetch::fetch_pin(&source, submodules)
        },
    }
}

struct PinResolution {
    outcome:  UpdateOutcome,
    node:     Option<LockedNode>,
    upstream: Option<FetchedTree>,
    patched:  Settled,
    /// the tag a pin with a template resolved to
    tag:      Option<String>,
    drift:    bool,
    warning:  Option<String>,
}

enum SignerMark {
    Keep,
    Set(SignedBy),
    Clear,
}

impl SignerMark {
    fn record_into(self, lock: &mut LockFile, pin: &str) -> bool {
        match self {
            Self::Keep => false,
            Self::Set(signed_by) => lock.set_signed_by(pin, Some(signed_by)),
            Self::Clear => lock.set_signed_by(pin, None),
        }
    }
}

/// a pin with signers only lands on a commit they signed, and every commit
/// since its verified anchor must be signed too, unless the pin rolls back to a
/// commit that chain already covered
fn check_signers(
    input: &pins::Input,
    old: Option<&LockedNode>,
    recorded: Option<&SignedBy>,
    keyring: &Keyring,
    resolution: &mut PinResolution,
) -> SignerMark {
    if input.signers.is_empty() {
        return if recorded.is_some() {
            SignerMark::Clear
        } else {
            SignerMark::Keep
        };
    }
    if matches!(resolution.outcome, UpdateOutcome::Failed(_)) {
        return SignerMark::Keep;
    }
    let Some(node) = resolution.node.as_ref().or(old) else {
        return SignerMark::Keep;
    };
    let listed = recorded.and_then(|record| {
        input
            .signers
            .iter()
            .find(|name| name.as_str() == record.signer)
            .map(|name| (record, name))
    });
    let trusted = listed.filter(|&(record, name)| {
        record.keys.is_some() && record.keys == keyring.keys_digest(name)
    });
    if let Some((_, name)) = listed
        && trusted.is_none()
    {
        let warning = format!("{name}'s keys changed since the last verified commit");
        resolution.warning = Some(
            resolution
                .warning
                .take()
                .map_or_else(|| warning.clone(), |prev| format!("{prev} {warning}")),
        );
    }
    let anchor = old
        .filter(|prev| {
            trusted.is_some() && SourceId::from_locked(prev) == SourceId::from_locked(node)
        })
        .and_then(LockedNode::forge_rev);
    let anchored = trusted.zip(anchor);
    if let Some(((record, _), rev)) = anchored
        && node.forge_rev() == Some(rev)
    {
        return SignerMark::Set(record.clone());
    }

    let start = anchored.map(|((record, _), rev)| {
        Anchor {
            rev,
            since: record.since.as_deref(),
        }
    });
    match keyring.verify(&input.signers, start, node) {
        Ok(Verdict {
            signer,
            rolled_back,
        }) => {
            if let Some(slot) = resolution.outcome.signed_by_mut() {
                *slot = Some(Signed {
                    signer: signer.to_string(),
                    rolled_back,
                });
            }
            let since = anchored
                .and_then(|((record, _), _)| record.since.clone())
                .or_else(|| node.forge_rev().map(str::to_owned));
            SignerMark::Set(SignedBy {
                signer: signer.to_string(),
                keys: keyring.keys_digest(signer),
                since,
            })
        },
        Err(err) => {
            resolution.outcome = UpdateOutcome::Failed(render::printable(&format!("{err:#}")));
            resolution.node = None;
            resolution.drift = false;
            SignerMark::Keep
        },
    }
}

impl PinResolution {
    fn tagged(mut self, previous: Option<&str>, chosen: Option<String>) -> Self {
        if let Some(picked) = chosen.as_deref() {
            self.outcome.show_tag(previous, picked);
        }
        self.tag = chosen;
        self
    }

    fn failed(err: &misstep::Report, warning: Option<String>) -> Self {
        Self {
            outcome: UpdateOutcome::Failed(format!("{err:#}")),
            node: None,
            upstream: None,
            patched: Settled::Current,
            tag: None,
            drift: false,
            warning,
        }
    }

    const fn frozen() -> Self {
        Self {
            outcome:  UpdateOutcome::Frozen,
            node:     None,
            upstream: None,
            patched:  Settled::Current,
            tag:      None,
            drift:    false,
            warning:  None,
        }
    }

    const fn unchanged(warning: Option<String>) -> Self {
        Self {
            outcome: UpdateOutcome::Unchanged,
            node: None,
            upstream: None,
            patched: Settled::Current,
            tag: None,
            drift: false,
            warning,
        }
    }

    fn drift(
        outcome: UpdateOutcome,
        node: LockedNode,
        accept: bool,
        warning: Option<String>,
    ) -> Self {
        Self {
            outcome,
            node: accept.then_some(node),
            upstream: None,
            patched: Settled::Current,
            tag: None,
            drift: !accept,
            warning,
        }
    }
}

fn classify(
    input: &pins::Input,
    expanded: &str,
    stale: bool,
    old: Option<&LockedNode>,
    accept: bool,
    warning: Option<String>,
    session: &CompareSession,
) -> PinResolution {
    let source = expanded.parse::<Source>().ok();
    let old_identity = old
        .and_then(LockedNode::resolved_identity)
        .map(LockIdentity::into_string);
    let old_compare_rev = old.filter(|_| !stale).and_then(comparable_rev);

    let resolved = if input.pin_type != PinType::Fixed
        && let Some(ref src) = source
    {
        session.resolve_and_compare(src, old_compare_rev).ok()
    } else {
        None
    };

    if let Some(ref current) = resolved
        && !stale
        && old_identity.as_deref() == Some(current.rev.as_str())
    {
        return PinResolution::unchanged(warning);
    }

    let mut fetched = match fetch_input(input.pin_type, input.unpack, input.submodules, expanded) {
        Ok(fetched) => fetched,
        Err(err) => return PinResolution::failed(&err, warning),
    };
    let upstream = fetched.take_tree();
    let (node, identity) = fetched.into_parts();
    let new_identity = String::from(identity);

    if old == Some(&node) {
        return PinResolution::unchanged(warning);
    }
    if input.pin_type == PinType::Fixed
        && !stale
        && old_identity.is_some()
        && old_identity.as_deref() != Some(new_identity.as_str())
        && locked_url(old) == locked_url(Some(&node))
    {
        return PinResolution::drift(
            UpdateOutcome::FixedDrift {
                old:      old_identity.unwrap_or_default(),
                new:      new_identity,
                accepted: accept,
            },
            node,
            accept,
            warning,
        );
    }
    if !stale && old_identity.as_deref() == Some(new_identity.as_str()) {
        return if hash_drifted(old, &node) {
            PinResolution::drift(
                UpdateOutcome::Drift {
                    rev:      new_identity,
                    accepted: accept,
                },
                node,
                accept,
                warning,
            )
        } else {
            PinResolution::unchanged(warning)
        };
    }

    let planned = resolved
        .filter(|current| current.rev == new_identity)
        .map_or_else(
            || {
                source
                    .as_ref()
                    .map_or_else(BranchComparison::default, |src| {
                        compare_with_planner(session, src, old_compare_rev, &new_identity)
                    })
            },
            |current| current.comparison,
        );

    let comparison = fetch::channel_repo(expanded)
        .zip(comparable_rev(&node))
        .map_or(planned, |(nixpkgs, new_rev)| {
            compare_with_planner(session, &nixpkgs, old_compare_rev, new_rev)
        });

    PinResolution {
        outcome: UpdateOutcome::Updated {
            old: old_identity,
            new: new_identity,
            comparison,
            dates: Dates::between(old, &node),
            signed_by: None,
        },
        node: Some(node),
        upstream,
        patched: Settled::Current,
        tag: None,
        drift: false,
        warning,
    }
}

/// what every pin's update reads and nothing writes until all have resolved
struct PinRun<'run> {
    project: &'run Project,
    lock:    &'run LockFile,
    keyring: &'run Keyring,
    session: &'run CompareSession,
    named:   &'run [String],
    accept:  bool,
}

impl PinRun<'_> {
    fn resolve(
        &self,
        input: &pins::Input,
        url: &str,
        fetching: impl FnOnce(),
    ) -> (PinResolution, SignerMark) {
        let lock = self.lock;
        let localized = source::localize_path_url_with_warning(url, self.project.dir());
        let old = lock.get(&input.name);
        let stale = declared_changed(input, lock, url, &localized.url);
        let frozen = input.frozen && !self.named.contains(&input.name) && old.is_some();
        // a frozen pin whose url was respelled relocks at the rev it holds
        let hold = old
            .and_then(LockedNode::forge_rev)
            .filter(|_| frozen && stale);
        if frozen && hold.is_none() {
            let mut resolution = PinResolution::frozen();
            if stale {
                resolution.warning = Some(format!(
                    "{0}: its url changed but it is frozen with no rev to relock at, run `tack \
                     update {0}` to move it",
                    input.name
                ));
            }
            return (resolution, SignerMark::Keep);
        }
        fetching();
        let target = match hold {
            Some(rev) => {
                Ok(Followed {
                    url: Cow::Owned(at_rev(&localized.url, rev)),
                    tag: lock.tag(&input.name).map(str::to_owned),
                })
            },
            None => tag::follow(&input.name, input.tag.as_ref(), &localized.url),
        };
        let mut resolution = match target {
            Ok(followed) => {
                classify(
                    input,
                    &followed.url,
                    stale,
                    old,
                    self.accept,
                    localized.warning,
                    self.session,
                )
                .tagged(lock.tag(&input.name), followed.tag)
            },
            Err(err) => PinResolution::failed(&err, localized.warning),
        };
        let recorded = lock.signed_by(&input.name);
        let mut mark = check_signers(input, old, recorded, self.keyring, &mut resolution);
        settle_patches(self.project, input, lock, &mut resolution);
        if matches!(resolution.outcome, UpdateOutcome::Failed(_)) {
            mark = SignerMark::Keep;
        }
        (resolution, mark)
    }
}

/// writes what a pin's update settled into the lock, returning whether the lock
/// changed and whether the pin moved to a new node
fn record_pin(
    lock: &mut LockFile,
    input: &pins::Input,
    url: &str,
    resolution: &mut PinResolution,
    mark: SignerMark,
) -> (bool, bool) {
    let resolved = !resolution.drift && !matches!(resolution.outcome, UpdateOutcome::Failed(_));
    let mut changed = resolved && lock.set_declared(&input.name, url);
    // a pin that failed keeps the tag of the rev it stays on
    let settled =
        resolution.node.is_some() || matches!(resolution.outcome, UpdateOutcome::Unchanged);
    let moved = resolution
        .node
        .take()
        .map(|node| lock.insert(input.name.clone(), node))
        .is_some();
    changed |= moved;
    changed |= mark.record_into(lock, &input.name);
    if settled {
        changed |= lock.set_tag(&input.name, resolution.tag.take());
    }
    changed |=
        mem::replace(&mut resolution.patched, Settled::Current).record_into(lock, &input.name);
    (changed, moved)
}

fn declared_changed(
    input: &pins::Input,
    lock: &LockFile,
    declared_url: &str,
    localized_url: &str,
) -> bool {
    let Some(node) = lock.get(&input.name) else {
        return false;
    };
    lock.declared(&input.name)
        .is_some_and(|recorded| recorded != declared_url)
        || !locked_from_source(input, localized_url.parse::<Source>().ok().as_ref(), node)
}

/// forgets what the lock and the gcroots keep for pins no longer declared
fn prune_departed(project: &Project, lock: &mut LockFile, all: &[pins::Input]) -> Result<bool> {
    let names = all
        .iter()
        .map(|input| input.name.as_str())
        .collect::<BTreeSet<_>>();
    patched::prune_roots(project, &names.iter().copied().collect::<Vec<_>>())?;
    Ok(lock.retain_declared(|name| names.contains(name)))
}

fn locked_from_source(input: &pins::Input, source: Option<&Source>, node: &LockedNode) -> bool {
    let Some(declared) = source else {
        return true;
    };
    if input.pin_type == PinType::Fixed {
        return true;
    }
    match *declared {
        Source::Tarball { .. } | Source::Path { .. } => return true,
        Source::Github { .. } | Source::Gitlab { .. } | Source::Git { .. } => {},
    }
    let locks_as_git = input.submodules || matches!(*declared, Source::Git { .. });
    let kind_matches = match *node {
        LockedNode::Git { submodules, .. } => locks_as_git && submodules == input.submodules,
        LockedNode::Github { .. } | LockedNode::Gitlab { .. } => !locks_as_git,
        LockedNode::Tarball { .. }
        | LockedNode::Fixed { .. }
        | LockedNode::Indirect { .. }
        | LockedNode::Path { .. } => false,
    };
    kind_matches && SourceId::from_locked(node) == Some(SourceId::from(declared.clone()))
}

fn locked_url(node: Option<&LockedNode>) -> Option<&str> {
    match node {
        Some(&LockedNode::Fixed { ref url, .. }) => url.as_deref(),
        Some(
            &LockedNode::Github { .. }
            | &LockedNode::Gitlab { .. }
            | &LockedNode::Git { .. }
            | &LockedNode::Tarball { .. }
            | &LockedNode::Indirect { .. }
            | &LockedNode::Path { .. },
        )
        | None => None,
    }
}

/// a new node only lands once its patches apply, so a conflict keeps the
/// pin on the rev it had
fn settle_patches(
    project: &Project,
    input: &pins::Input,
    lock: &LockFile,
    resolution: &mut PinResolution,
) {
    let upstream = resolution.upstream.take();
    if matches!(resolution.outcome, UpdateOutcome::Failed(_)) {
        return;
    }
    let moved = resolution.node.is_some();
    let Some(node) = resolution.node.as_ref().or_else(|| lock.get(&input.name)) else {
        return;
    };
    let pin = PatchedPin::new(project, &input.name, node).with_upstream(upstream);
    match pin.settle(
        &input.patches,
        lock.patched(&input.name),
        moved,
        Mode::Update,
    ) {
        Ok(settled) => {
            // the rev line says unchanged, so a rebuild on the same node needs saying
            let rebuilt = match settled {
                Settled::Rehashed(_) => Some("its tree no longer matched the lock's hash"),
                Settled::Rebuilt(_) if !moved => Some("its patches changed"),
                Settled::Current | Settled::Restored | Settled::Rebuilt(_) | Settled::Unpatched => {
                    None
                },
            };
            if let Some(reason) = rebuilt {
                let message = format!("{}: rebuilt its patched tree, {reason}", input.name);
                resolution.warning = Some(
                    resolution
                        .warning
                        .take()
                        .map_or_else(|| message.clone(), |prev| format!("{prev} {message}")),
                );
            }
            resolution.patched = settled;
        },
        Err(err) => {
            resolution.outcome = UpdateOutcome::Failed(format!("{err:#}"));
            resolution.node = None;
        },
    }
}

/// `url` pinned to `rev`, unless it already names a rev of its own
fn at_rev(url: &str, rev: &str) -> String {
    let named = url.parse::<Source>().is_ok_and(|source| {
        matches!(
            source,
            Source::Github { rev: Some(_), .. }
                | Source::Gitlab { rev: Some(_), .. }
                | Source::Git { rev: Some(_), .. }
        )
    });
    if named {
        return url.to_owned();
    }
    let (base, fragment) = url.find('#').map_or((url, ""), |at| url.split_at(at));
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}rev={rev}{fragment}")
}

fn comparable_rev(node: &LockedNode) -> Option<&str> {
    match *node {
        LockedNode::Github {
            rev: Some(ref rev), ..
        }
        | LockedNode::Gitlab {
            rev: Some(ref rev), ..
        }
        | LockedNode::Git {
            rev: Some(ref rev), ..
        }
        | LockedNode::Tarball {
            rev: Some(ref rev), ..
        } => Some(rev),
        LockedNode::Github { rev: None, .. }
        | LockedNode::Gitlab { rev: None, .. }
        | LockedNode::Git { rev: None, .. }
        | LockedNode::Tarball { rev: None, .. }
        | LockedNode::Fixed { .. }
        | LockedNode::Indirect { .. }
        | LockedNode::Path { .. } => None,
    }
}

fn compare_with_planner(
    session: &CompareSession,
    source: &Source,
    old_rev: Option<&str>,
    new_rev: &str,
) -> BranchComparison {
    let Some(previous_rev) = old_rev else {
        return BranchComparison::default();
    };
    if previous_rev == new_rev {
        return BranchComparison::verified(CompareStatus::Identical);
    }
    let Some(job) = CompareJob::from_source(source, previous_rev, new_rev) else {
        return BranchComparison::default();
    };
    session
        .compare(&job)
        .status
        .map_or_else(BranchComparison::unavailable, BranchComparison::verified)
}

fn hash_drifted(old: Option<&LockedNode>, node: &LockedNode) -> bool {
    matches!(
        (old.and_then(LockedNode::hash), node.hash()),
        (Some(prev), Some(curr)) if prev != curr
    )
}

pub(super) fn update(
    project: &Project,
    selection: Selection<'_>,
    accept: bool,
    progress: &impl Progress<UpdateOutcome>,
) -> Result<UpdateReport> {
    let doc = project.load_pins()?;
    let shorturls = doc.shorturls();
    let all = doc.inputs()?;
    let all_follow = doc.all_follows()?;
    let selected = select(&all, selection)?;
    if selected.is_empty() {
        return Ok(UpdateReport::default());
    }
    let jobs = selected
        .iter()
        .map(|input| {
            shorturls
                .expand(&input.url)
                .map(|expanded| (*input, expanded))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut lock = project.load_lock()?;
    let keyring = doc.keyring(&selected, project.dir())?;
    progress.begin(&selected);

    let session = CompareSession::new();
    let run = PinRun {
        project,
        lock: &lock,
        keyring: &keyring,
        session: &session,
        named: selection.names,
        accept,
    };
    let resolutions = dispatcher::ordered(jobs, UPDATE_IN_FLIGHT, |index, (input, url)| {
        let (resolution, mark) = run.resolve(input, &url, || progress.fetching(index));
        progress.finished(index, &resolution.outcome);
        (resolution, mark, url)
    });

    let mut changed = false;
    let mut drift = 0_usize;
    let mut pins = Vec::with_capacity(resolutions.len());
    let mut warnings = Vec::new();
    let mut changed_names = Vec::new();
    for (input, (mut resolution, mark, url)) in selected.iter().zip(resolutions) {
        warnings.extend(resolution.warning.take());
        let (recorded, moved) = record_pin(&mut lock, input, &url, &mut resolution, mark);
        changed |= recorded;
        if moved {
            changed_names.push(input.name.clone());
        }
        if resolution.drift {
            drift += 1;
        }
        pins.push(PinUpdate {
            name:    input.name.clone(),
            outcome: resolution.outcome,
        });
    }

    changed |= prune_departed(project, &mut lock, &all)?;

    let auto_dedup = if drift == 0 && !changed_names.is_empty() {
        dedup::auto_dedup_scoped(&all, &all_follow, &mut lock, &changed_names)
    } else {
        dedup::AutoDedupReport::default()
    };
    if auto_dedup.changed {
        changed = true;
    }
    if changed {
        project.save_lock(&lock)?;
    }

    for diagnostic in auto_dedup.scan_diagnostics {
        warnings.push(render::scan_diagnostic(&diagnostic));
    }
    warnings.extend(session.into_surfaced());
    warnings.extend(auto_dedup.surfaced_fetch_causes);
    warnings.extend(fetch::drain_fetch_warnings());

    Ok(UpdateReport {
        pins,
        drift,
        warnings,
    })
}

fn classify_look(
    input: &pins::Input,
    expanded: &str,
    old_identity: Option<&str>,
    old_compare_rev: Option<&str>,
    stale: bool,
    verbose: bool,
    session: &CompareSession,
) -> (LookOutcome, Option<CommitLog>) {
    if input.pin_type == PinType::Fixed {
        return (
            LookOutcome::Skipped("fixed pin, run `tack update` to verify".to_owned()),
            None,
        );
    }
    let source = match expanded.parse::<Source>() {
        Ok(source) => source,
        Err(err) => return (LookOutcome::Failed(format!("{err:#}")), None),
    };
    if matches!(source, Source::Path { .. }) {
        return (LookOutcome::Skipped("local path".to_owned()), None);
    }
    match session.resolve_and_compare(&source, old_compare_rev) {
        Ok(current) if !stale && old_identity == Some(current.rev.as_str()) => {
            (LookOutcome::Unchanged, None)
        },
        Ok(current) => {
            let channel = matches!(source, Source::Tarball { .. })
                .then(|| fetch::channel_repo(&current.rev).zip(fetch::channel_rev(&current.rev)))
                .flatten();
            let (history, head, comparison) = match channel {
                Some((ref nixpkgs, ref rev)) => {
                    let comparison = compare_with_planner(session, nixpkgs, old_compare_rev, rev);
                    (nixpkgs, rev.as_str(), comparison)
                },
                None => (&source, current.rev.as_str(), current.comparison),
            };
            let log = match (verbose, old_compare_rev) {
                (true, Some(old_rev)) => {
                    fetch::commits_between(history, old_rev, head, LOG_LIMIT)
                        .ok()
                        .flatten()
                },
                _ => None,
            };
            (
                LookOutcome::Updated {
                    old: old_identity.map(str::to_owned),
                    new: current.rev,
                    comparison,
                },
                log,
            )
        },
        Err(err) => (LookOutcome::Failed(format!("{err:#}")), None),
    }
}

pub(super) fn look(
    project: &Project,
    selection: Selection<'_>,
    verbose: bool,
    progress: &impl Progress<LookOutcome>,
) -> Result<LookReport> {
    let doc = project.load_pins()?;
    let shorturls = doc.shorturls();
    let all = doc.inputs()?;
    let selected = select(&all, selection)?;
    if selected.is_empty() {
        return Ok(LookReport::default());
    }
    let jobs = selected
        .iter()
        .map(|input| {
            shorturls
                .expand(&input.url)
                .map(|expanded| (*input, expanded))
        })
        .collect::<Result<Vec<_>>>()?;
    let lock = project.load_lock()?;
    progress.begin(&selected);

    let session = CompareSession::new();
    let look_results = dispatcher::ordered(jobs, LOOK_IN_FLIGHT, |index, (input, url)| {
        progress.fetching(index);
        let localized = source::localize_path_url_with_warning(&url, project.dir());
        let old = lock
            .get(&input.name)
            .and_then(LockedNode::resolved_identity)
            .map(LockIdentity::into_string);
        let stale = declared_changed(input, &lock, &url, &localized.url);
        let old_compare_rev = lock
            .get(&input.name)
            .filter(|_| !stale)
            .and_then(comparable_rev);
        let (mut outcome, log, chosen) =
            match tag::follow(&input.name, input.tag.as_ref(), &localized.url) {
                Ok(followed) => {
                    let (outcome, log) = classify_look(
                        input,
                        &followed.url,
                        old.as_deref(),
                        old_compare_rev,
                        stale,
                        verbose,
                        &session,
                    );
                    (outcome, log, followed.tag)
                },
                Err(err) => (LookOutcome::Failed(format!("{err:#}")), None, None),
            };
        let mut warnings = Vec::from_iter(localized.warning);
        let pulls = match (lock.get(&input.name), lock.patched(&input.name)) {
            (Some(node), Some(tree)) => {
                if patched::drifted(project, &input.patches, tree) {
                    warnings.push(format!(
                        "{0}: its patches no longer match the lock, run `tack update {0}`",
                        input.name
                    ));
                }
                let upstream = match outcome {
                    LookOutcome::Updated { ref new, .. } => Some(new.as_str()),
                    LookOutcome::Unchanged => old_compare_rev,
                    LookOutcome::Skipped(_) | LookOutcome::Failed(_) | LookOutcome::Tagged(_) => {
                        None
                    },
                };
                let (found, unchecked) =
                    patched::pull_patches(&input.patches, tree, node, upstream);
                warnings.extend(
                    unchecked
                        .into_iter()
                        .map(|reason| format!("{}: could not check {reason}", input.name)),
                );
                found
            },
            _ => Vec::new(),
        };
        // after the pull status check, which needs the upstream rev, not the tag
        if let Some(picked) = chosen.as_deref() {
            outcome.show_tag(lock.tag(&input.name), picked);
        }
        progress.finished(index, &outcome);
        (
            PinLook {
                name: input.name.clone(),
                outcome,
                log,
                pulls,
            },
            warnings,
        )
    });

    let mut warnings = Vec::new();
    let pins = look_results
        .into_iter()
        .map(|(pin, pin_warnings)| {
            warnings.extend(pin_warnings);
            pin
        })
        .collect::<Vec<PinLook>>();
    warnings.extend(session.into_surfaced());
    warnings.extend(fetch::drain_fetch_warnings());

    Ok(LookReport { pins, warnings })
}
