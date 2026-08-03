// SPDX-License-Identifier: EUPL-1.2

use pound::Parse;

use crate::pins::{
    PinType,
    Unpack,
};

/// the parsed subcommand, in the shape the rest of tack consumes
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Init {
        force:    bool,
        resolver: bool,
        flake:    bool,
        convert:  bool,
    },
    Update {
        names:  Vec<String>,
        accept: bool,
    },
    Look {
        names:   Vec<String>,
        verbose: bool,
    },
    Add {
        name:       String,
        url:        String,
        pin_type:   PinType,
        unpack:     Option<Unpack>,
        dir:        Option<String>,
        submodules: bool,
        follows:    Vec<(String, String)>,
    },
    Rm {
        name: String,
    },
    Alias {
        name:     String,
        template: Option<String>,
        rm:       bool,
    },
    Dedup,
    Undo {
        list: bool,
    },
    Redo,
}

/// flake-like toml nix pins, lazily fetched and transformed
#[derive(Parse)]
#[pound(name = "tack")]
enum Cli {
    /// scaffold a .tack dir (default.nix, pins.toml, pins.lock.json)
    Init {
        /// overwrite tack-managed files that already exist
        #[pound(long)]
        force:    bool,
        /// refresh only the resolver (default.nix)
        #[pound(long)]
        resolver: bool,
        /// also scaffold a recomposable flake.nix
        #[pound(long)]
        flake:    bool,
        /// import inputs from an existing flake.nix into pins.toml
        #[pound(long)]
        convert:  bool,
    },
    /// fetch pins and rewrite the lock
    Update {
        /// relock drifted pins instead of failing
        #[pound(long)]
        accept: bool,
        /// pins to update (default: all)
        names:  Vec<String>,
    },
    /// show upstream drift without writing the lock
    Look {
        /// list the freshest commits for each changed pin
        #[pound(short, long)]
        verbose: bool,
        /// pins to inspect (default: all)
        names:   Vec<String>,
    },
    /// add a pin
    Add {
        /// input name
        name:       String,
        /// pin url (shorturl ok; ?rev=<sha> pins a commit)
        url:        String,
        /// pin a source tree only, no flake eval
        #[pound(long, group = "kind")]
        fetch:      bool,
        /// pin a fixed-output derivation
        #[pound(long, group = "kind")]
        fixed:      bool,
        /// for --fixed: how to materialise the download
        #[pound(long)]
        unpack:     Option<Unpack>,
        /// subdir holding flake.nix
        #[pound(long)]
        dir:        Option<String>,
        /// fetch git submodules
        #[pound(long)]
        submodules: bool,
        /// follows child=parent (repeatable; a bare child follows its namesake)
        #[pound(long)]
        follows:    Vec<String>,
    },
    /// remove a pin
    Rm {
        /// input name
        name: String,
    },
    /// define or remove a shorturl alias
    Alias {
        /// alias name
        name:     String,
        /// expansion template containing {path}
        template: Option<String>,
        /// remove the alias instead of defining it
        #[pound(long)]
        rm:       bool,
    },
    /// collapse duplicate pins onto a single source
    Dedup,
    /// revert the last tack edit
    Undo {
        /// list the undo history
        #[pound(long)]
        list: bool,
    },
    /// reapply an undone edit
    Redo,
}

pub fn parse() -> Command {
    Cli::parse().into()
}

impl Command {
    pub fn history_label(&self) -> String {
        match *self {
            Self::Init {
                resolver,
                convert,
                flake,
                force,
            } => {
                if resolver {
                    "init --resolver"
                } else if convert {
                    "init --convert"
                } else if flake {
                    "init --flake"
                } else if force {
                    "init --force"
                } else {
                    "init"
                }
                .to_owned()
            },
            Self::Update { ref names, .. } => {
                if names.is_empty() {
                    "update".to_owned()
                } else {
                    format!("update {}", names.join(" "))
                }
            },
            Self::Add { ref name, .. } => format!("add {name}"),
            Self::Rm { ref name } => format!("rm {name}"),
            Self::Alias { ref name, rm, .. } => {
                if rm {
                    format!("alias --rm {name}")
                } else {
                    format!("alias {name}")
                }
            },
            Self::Look { .. } | Self::Dedup | Self::Undo { .. } | Self::Redo => String::new(),
        }
    }
}

impl From<Cli> for Command {
    fn from(cli: Cli) -> Self {
        match cli {
            Cli::Init {
                force,
                resolver,
                flake,
                convert,
            } => {
                Self::Init {
                    force,
                    resolver,
                    flake,
                    convert,
                }
            },
            Cli::Update { accept, names } => Self::Update { names, accept },
            Cli::Look { verbose, names } => Self::Look { names, verbose },
            Cli::Add {
                name,
                url,
                fetch,
                fixed,
                unpack,
                dir,
                submodules,
                follows,
            } => {
                let pin_type = if fixed {
                    PinType::Fixed
                } else if fetch {
                    PinType::Fetch
                } else {
                    PinType::Flake
                };
                Self::Add {
                    name,
                    url,
                    pin_type,
                    unpack,
                    dir,
                    submodules,
                    follows: follows.iter().map(|rule| parse_follows(rule)).collect(),
                }
            },
            Cli::Rm { name } => Self::Rm { name },
            Cli::Alias { name, template, rm } => Self::Alias { name, template, rm },
            Cli::Dedup => Self::Dedup,
            Cli::Undo { list } => Self::Undo { list },
            Cli::Redo => Self::Redo,
        }
    }
}

fn parse_follows(rule: &str) -> (String, String) {
    match rule.split_once('=') {
        Some((child, parent)) => (child.to_owned(), parent.to_owned()),
        None => (rule.to_owned(), rule.to_owned()),
    }
}
