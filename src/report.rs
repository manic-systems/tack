// SPDX-License-Identifier: EUPL-1.2

use std::collections::{
    BTreeMap,
    BTreeSet,
};

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
