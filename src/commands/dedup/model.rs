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
    pub path: Vec<String>,
    pub name: String,
    pub side: Side,
    pub rev:  String,
    pub lm:   Option<u64>,
}
