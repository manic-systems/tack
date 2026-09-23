// SPDX-License-Identifier: EUPL-1.2

use std::collections::{
    BTreeMap,
    BTreeSet,
};

use crate::fetch::{
    BranchComparison,
    CommitLog,
};

#[derive(Clone, Debug)]
pub enum UpdateOutcome {
    Unchanged,
    Updated {
        old:        Option<String>,
        new:        String,
        comparison: BranchComparison,
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
}

#[derive(Clone, Debug, Default)]
pub struct LookReport {
    pub pins:     Vec<PinLook>,
    pub warnings: Vec<String>,
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
