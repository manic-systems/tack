// SPDX-License-Identifier: EUPL-1.2

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    Flake,
    Tack,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match *self {
            Self::Flake => "flake",
            Self::Tack => "tack",
        })
    }
}

pub(super) struct Entry {
    pub path: Vec<String>,
    pub name: String,
    pub side: Side,
    pub rev:  String,
    pub lm:   Option<u64>,
}
