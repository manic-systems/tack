// SPDX-License-Identifier: EUPL-1.2

/// which side of an upstream a finding came from
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    Flake,
    Tack,
}

impl Side {
    pub(super) const fn as_str(self) -> &'static str {
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
