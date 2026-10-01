// SPDX-License-Identifier: EUPL-1.2

use std::str::FromStr;

/// the commits a pin would move across, newest first
#[derive(Clone, Debug)]
pub struct CommitLog {
    pub fresh:  Vec<(String, String)>,
    pub base:   Option<(String, String)>,
    pub total:  usize,
    pub ahead:  u64,
    pub behind: u64,
}

#[derive(Clone, Debug)]
pub struct CommitObject {
    pub id:   String,
    pub data: Vec<u8>,
}

/// the commits a pin moves across, from a verified anchor to its new rev
#[derive(Clone, Debug)]
pub enum CommitRange {
    /// every commit reachable from the new rev but not from the anchor
    Commits(Vec<CommitObject>),
    /// the anchor is not an ancestor of the new rev
    Diverged,
    /// the new rev is the anchor's own ancestor, so the pin moves back
    Ancestor,
    /// the range doesn't fit in the fetch limits
    TooLarge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareStatus {
    Ahead,
    Behind,
    Diverged,
    Identical,
}

impl FromStr for CompareStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "ahead" => Self::Ahead,
            "behind" => Self::Behind,
            "diverged" => Self::Diverged,
            "identical" => Self::Identical,
            _ => return Err(()),
        })
    }
}

#[cfg(test)]
impl CompareStatus {
    pub const fn from_ancestry(
        base_is_ancestor_of_head: bool,
        head_is_ancestor_of_base: bool,
    ) -> Self {
        match (base_is_ancestor_of_head, head_is_ancestor_of_base) {
            (true, true) => Self::Identical,
            (true, false) => Self::Ahead,
            (false, true) => Self::Behind,
            (false, false) => Self::Diverged,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BranchComparison {
    pub status:   Option<CompareStatus>,
    pub expected: bool,
}

impl BranchComparison {
    pub const fn verified(status: CompareStatus) -> Self {
        Self {
            status:   Some(status),
            expected: true,
        }
    }

    pub const fn unavailable() -> Self {
        Self {
            status:   None,
            expected: true,
        }
    }
}

pub struct CurrentRev {
    pub rev:        String,
    pub comparison: BranchComparison,
}
