// SPDX-License-Identifier: EUPL-1.2

use std::io::{
    self,
    IsTerminal as _,
};

#[derive(Clone, Copy)]
pub enum Sgr {
    Bold,
    Dim,
    Red,
    Green,
    Yellow,
    Blue,
    Cyan,
}

impl Sgr {
    const fn code(self) -> u8 {
        match self {
            Self::Bold => 1,
            Self::Dim => 2,
            Self::Red => 31,
            Self::Green => 32,
            Self::Yellow => 33,
            Self::Blue => 34,
            Self::Cyan => 36,
        }
    }

    /// paints unconditionally, for output that only ever reaches a terminal
    pub fn wrap(self, text: &str) -> String {
        format!("\x1b[{}m{text}\x1b[0m", self.code())
    }
}

/// paints only when stdout is a terminal, so piped output stays plain
#[derive(Clone, Copy)]
pub struct Style {
    tty: bool,
}

impl Style {
    pub fn stdout() -> Self {
        Self {
            tty: io::stdout().is_terminal(),
        }
    }

    pub const fn is_tty(self) -> bool {
        self.tty
    }

    pub fn paint(self, sgr: Sgr, text: &str) -> String {
        if self.tty {
            sgr.wrap(text)
        } else {
            text.to_owned()
        }
    }
}
