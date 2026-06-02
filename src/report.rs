// SPDX-License-Identifier: EUPL-1.2

use std::collections::{
    BTreeMap,
    BTreeSet,
};

use crate::fetch::github::{
    BranchComparison,
    CommitLog,
};

/// what `update` did to one pin
#[derive(Clone, Debug)]
pub enum UpdateOutcome {
    /// lock already matched upstream
    Unchanged,
    /// relocked to a new rev; `old` is `None` for a freshly added pin
    Updated {
        old:        Option<String>,
        new:        String,
        comparison: BranchComparison,
    },
    /// rev is stable but content moved under it
    Drift { rev: String, accepted: bool },
    /// a fixed pin's sha256 changed
    FixedDrift {
        old:      String,
        new:      String,
        accepted: bool,
    },
    /// the fetch failed; the lock was left untouched
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct PinUpdate {
    pub name:    String,
    pub outcome: UpdateOutcome,
}

/// the result of an `update` run: per-pin outcomes plus run-wide totals
#[derive(Clone, Debug, Default)]
pub struct UpdateReport {
    pub pins:     Vec<PinUpdate>,
    /// pins whose content drifted and were kept (not relocked)
    pub drift:    usize,
    /// non-fatal notices a caller may surface (token hints, dedup diagnostics)
    pub warnings: Vec<String>,
}

/// what `look` saw for one pin, without touching the lock
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
    /// freshest commits, only populated for a verbose look
    pub log:     Option<CommitLog>,
}

/// the result of a `look` run
#[derive(Clone, Debug, Default)]
pub struct LookReport {
    pub pins:     Vec<PinLook>,
    pub warnings: Vec<String>,
}

/// dedup scan result: only diverging groups, plus follow suggestions
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DedupReport {
    pub groups:  Vec<DedupGroup>,
    pub follows: FollowSuggestions,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FollowSuggestions {
    /// groups that have a top-level pin
    pub pin:  FollowMap,
    /// transitive-only groups
    pub auto: FollowMap,
}

impl FollowSuggestions {
    pub fn is_empty(&self) -> bool {
        self.pin.is_empty() && self.auto.is_empty()
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

    pub fn insert(&mut self, alias: String, target: String) -> Option<String> {
        self.aliases.insert(alias, target)
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

/// rev position relative to its group comparator
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
