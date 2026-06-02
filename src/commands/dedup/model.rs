// SPDX-License-Identifier: EUPL-1.2

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
    /// lineage from top pin down to the parent tree scanned
    pub path: Vec<String>,
    pub name: String,
    /// for side-scoped follow matching
    pub side: Side,
    /// untruncated
    pub rev:  String,
    /// node lastModified
    pub lm:   Option<u64>,
}
