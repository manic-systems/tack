// SPDX-License-Identifier: EUPL-1.2

//! cross-backend branch topology model shared by every git forge

use std::str::FromStr;

/// direction of head relative to base, per a forge's compare endpoint
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareStatus {
    /// head has commits base lacks
    Ahead,
    /// head is missing commits base has
    Behind,
    /// each side has unique commits
    Diverged,
    /// same commit
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
    pub const fn none() -> Self {
        Self {
            status:   None,
            expected: false,
        }
    }

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
