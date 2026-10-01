// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    fmt::{
        Display,
        Formatter,
        Result as FmtResult,
    },
};

use crate::{
    fetch::{
        BranchComparison,
        CommitLog,
    },
    lock::LockedNode,
    render,
};

#[derive(Clone, Debug)]
pub enum UpdateOutcome {
    Unchanged,
    Updated {
        old:        Option<String>,
        new:        String,
        comparison: BranchComparison,
        /// the signer of the new rev, for pins with `signers`
        signed_by:  Option<Signed>,
    },
    Drift {
        rev:      String,
        accepted: bool,
    },
    FixedDrift {
        old:      String,
        new:      String,
        accepted: bool,
    },
    Frozen,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct PinUpdate {
    pub name:    String,
    pub outcome: UpdateOutcome,
}

#[derive(Clone, Debug, Default)]
pub struct UpdateReport {
    pub pins:     Vec<PinUpdate>,
    pub drift:    usize,
    pub warnings: Vec<String>,
}

impl UpdateReport {
    pub fn user_error(&self) -> Option<String> {
        let failed = self
            .pins
            .iter()
            .filter(|pin| matches!(pin.outcome, UpdateOutcome::Failed(_)))
            .count();
        if failed > 0 {
            return Some(format!("{failed} pin(s) failed to update"));
        }
        (self.drift > 0).then(|| {
            "upstream content differs from lock (drifted pins kept; investigate, then re-run with \
             --accept to relock)"
                .to_owned()
        })
    }
}

/// who signed a pin's new rev, and whether the pin moved back to a commit its
/// verified chain already covered
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    pub signer:      String,
    pub rolled_back: bool,
}

impl Display for Signed {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "signed by {}", self.signer)?;
        if self.rolled_back {
            f.write_str(" (rolled back to an earlier verified commit)")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyOutcome {
    Signed {
        signer: String,
        notes:  Vec<VerifyNote>,
    },
    /// the pin is gone and had signers at the base, so nothing is built from it
    Removed,
    Failed(String),
}

/// what made a passing check weaker than the full range from the base, or
/// changed who is trusted
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyNote {
    NewPin,
    SourceChanged,
    NoAnchor,
    NoBase,
    KeysChanged(Vec<String>),
    TipOnly,
    SignersAdded(Vec<String>),
    RolledBack,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinVerify {
    pub name:    String,
    pub outcome: VerifyOutcome,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub pins: Vec<PinVerify>,
}

impl VerifyReport {
    pub fn user_error(&self) -> Option<String> {
        let failed = self
            .pins
            .iter()
            .filter(|pin| matches!(pin.outcome, VerifyOutcome::Failed(_)))
            .count();
        (failed > 0).then(|| format!("{failed} pin(s) failed signature checks"))
    }
}

#[derive(Clone, Debug)]
pub enum LookOutcome {
    Unchanged,
    Updated {
        old:        Option<String>,
        new:        String,
        comparison: BranchComparison,
    },
    Skipped(String),
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct PinLook {
    pub name:    String,
    pub outcome: LookOutcome,
    pub log:     Option<CommitLog>,
    pub pulls:   Vec<PullPatch>,
}

/// a vendored pull request that needs attention, open ones that haven't moved
/// are left out
#[derive(Clone, Debug)]
pub struct PullPatch {
    pub source:    String,
    pub reference: PatchRef,
    pub status:    PullStatus,
}

/// what a remote patch came from, written the way its forge writes it
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchRef {
    PullRequest(u64),
    MergeRequest(u64),
    Commit(String),
}

impl Display for PatchRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match *self {
            Self::PullRequest(number) => write!(f, "PR #{number}"),
            Self::MergeRequest(number) => write!(f, "MR !{number}"),
            Self::Commit(ref rev) => write!(f, "commit {}", rev.get(..7).unwrap_or(rev)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullStatus {
    /// merged, and upstream's newest rev has it
    Landed,
    /// merged but not in the pin yet, or not checkable when the pin is a
    /// tarball, a fork, or the forge names no merge commit
    Merged {
        checked: bool,
    },
    Closed,
    /// open, and its head is not the one that was vendored
    Changed,
}

#[derive(Clone, Debug, Default)]
pub struct LookReport {
    pub pins:     Vec<PinLook>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct TreeReport {
    pub pins:     Vec<PinTree>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct PinTree {
    pub name:   String,
    pub group:  Option<String>,
    pub lock:   PinLock,
    pub inputs: Vec<TreeInput>,
}

#[derive(Clone, Debug)]
pub enum PinLock {
    /// until `tack update` locks the pin
    Missing,
    Locked(LockedSource),
    /// a lock type this tack cannot read, by name
    Unknown(String),
}

#[derive(Clone, Debug)]
pub struct TreeInput {
    pub name:   String,
    pub target: TreeTarget,
}

#[derive(Clone, Debug)]
pub enum TreeTarget {
    Locked {
        source: LockedSource,
        inputs: Vec<TreeInput>,
    },
    /// a node whose inputs were already listed under its first appearance
    Repeated(LockedSource),
    /// a lock type this tack cannot read, by name
    Unknown(String),
    /// a path of input names from the root of the pin's flake.lock
    FollowsInput(Vec<String>),
    FollowsPin(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LockedSource {
    pub url:           String,
    pub rev:           Option<String>,
    pub last_modified: Option<u64>,
}

impl From<&LockedNode> for LockedSource {
    fn from(node: &LockedNode) -> Self {
        let url = match *node {
            LockedNode::Github {
                ref owner,
                ref repo,
                ..
            } => format!("github:{owner}/{repo}"),
            LockedNode::Gitlab {
                ref host,
                ref owner,
                ref repo,
                ..
            } => format!("gitlab:{owner}/{repo}?host={host}"),
            LockedNode::Git { ref url, .. } => format!("git+{url}"),
            LockedNode::Tarball { ref url, .. } => url.clone(),
            LockedNode::Path { ref path, .. } => format!("path:{path}"),
            LockedNode::Indirect { ref id, .. } => format!("flake:{id}"),
            LockedNode::Fixed { ref url, .. } => url.clone().unwrap_or_else(|| "fixed".to_owned()),
        };
        Self {
            url,
            rev: node.forge_rev().map(render::short),
            last_modified: node.last_modified().filter(|&modified| modified > 0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DedupReport {
    pub groups:  Vec<DedupGroup>,
    pub follows: FollowSuggestions,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FollowSuggestions {
    pub pin:       FollowMap,
    pub auto:      FollowMap,
    /// names whose groups disagree on a target
    pub conflicts: BTreeSet<String>,
}

#[derive(Clone, Copy)]
pub enum FollowKind {
    Pin,
    Auto,
}

impl FollowSuggestions {
    pub fn is_empty(&self) -> bool {
        self.pin.is_empty() && self.auto.is_empty() && self.conflicts.is_empty()
    }

    /// `[all_follow]` matches by name across the whole graph, so a name two
    /// groups point elsewhere is left for the user instead of letting the
    /// last group win
    pub(crate) fn suggest(&mut self, kind: FollowKind, alias: &str, target: &str) {
        if self.conflicts.contains(alias) {
            return;
        }
        let previous = self
            .pin
            .aliases
            .get(alias)
            .or_else(|| self.auto.aliases.get(alias));
        match previous {
            Some(existing) if existing != target => {
                self.pin.aliases.remove(alias);
                self.auto.aliases.remove(alias);
                self.conflicts.insert(alias.to_owned());
            },
            Some(_) => {},
            None => {
                let map = match kind {
                    FollowKind::Pin => &mut self.pin,
                    FollowKind::Auto => &mut self.auto,
                };
                map.aliases.insert(alias.to_owned(), target.to_owned());
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FollowMap {
    aliases: BTreeMap<String, String>,
}

impl FollowMap {
    pub fn is_empty(&self) -> bool {
        self.aliases.is_empty()
    }

    pub fn collapsed(&self) -> Vec<CollapsedFollow> {
        let mut by_target = BTreeMap::<&str, BTreeSet<&str>>::new();
        for (alias, target) in &self.aliases {
            by_target
                .entry(target.as_str())
                .or_default()
                .insert(alias.as_str());
        }
        let mut lines = Vec::<CollapsedFollow>::new();
        for (target, aliases) in &by_target {
            if aliases.len() == 1 {
                let alias = aliases.iter().next().copied().unwrap_or("");
                lines.push(CollapsedFollow::Single {
                    alias:  alias.to_owned(),
                    target: (*target).to_owned(),
                });
            } else {
                let collapsed_aliases = aliases
                    .iter()
                    .filter(|alias| **alias != *target)
                    .map(|alias| (*alias).to_owned())
                    .collect::<Vec<_>>();
                lines.push(CollapsedFollow::Group {
                    target:  (*target).to_owned(),
                    aliases: collapsed_aliases,
                });
            }
        }
        lines
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CollapsedFollow {
    Single {
        alias:  String,
        target: String,
    },
    Group {
        target:  String,
        aliases: Vec<String>,
    },
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
    pub sources: Vec<Vec<String>>,
}

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
