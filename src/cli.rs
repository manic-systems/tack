// SPDX-License-Identifier: EUPL-1.2

use anyhow::{
    Context as _,
    Result,
    bail,
};

use crate::pins::{
    PinType,
    Unpack,
};

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Init {
        force: bool,
    },
    Update {
        names:  Vec<String>,
        accept: bool,
    },
    Look {
        names: Vec<String>,
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
    Dedup {
        deep: bool,
    },
    Help,
}

pub fn parse() -> Result<Command> {
    parse_parser(lexopt::Parser::from_env())
}

#[expect(clippy::too_many_lines, reason = "it's a simple parser")]
fn parse_parser(mut parser: lexopt::Parser) -> Result<Command> {
    use lexopt::prelude::*;

    let sub = match parser.next()? {
        Some(Value(value)) => {
            value
                .string()
                .map_err(|_| anyhow::anyhow!("invalid subcommand"))?
        },
        Some(Long("help") | Short('h')) | None => return Ok(Command::Help),
        Some(arg) => return Err(arg.unexpected().into()),
    };

    match sub.as_str() {
        "init" => {
            let mut force = false;
            while let Some(arg) = parser.next()? {
                match arg {
                    Long("force") => force = true,
                    Short(_) | Long(_) | Value(_) => return Err(arg.unexpected().into()),
                }
            }
            Ok(Command::Init { force })
        },
        "update" | "look" => {
            let mut names = Vec::new();
            let mut accept = false;
            while let Some(arg) = parser.next()? {
                match arg {
                    Long("accept") if sub == "update" => accept = true,
                    Value(value) => {
                        names.push(value.string().map_err(|_| anyhow::anyhow!("bad name"))?);
                    },
                    Short(_) | Long(_) => return Err(arg.unexpected().into()),
                }
            }
            Ok(if sub == "update" {
                Command::Update { names, accept }
            } else {
                Command::Look { names }
            })
        },
        "add" => {
            let (mut name, mut url) = (None, None);
            let mut pin_type = PinType::Flake;
            let mut unpack = Option::<Unpack>::None;
            let mut dir = None;
            let mut submodules = false;
            let mut follows = Vec::new();
            while let Some(arg) = parser.next()? {
                match arg {
                    Long("no-flake") => pin_type = PinType::Fetch,
                    Long("fetch") => pin_type = PinType::Fetch,
                    Long("fixed") => pin_type = PinType::Fixed,
                    Long("unpack") => {
                        let value = parser
                            .value()?
                            .string()
                            .map_err(|_| anyhow::anyhow!("bad unpack"))?;
                        unpack = Some(match value.as_str() {
                            "tarball" => Unpack::Tarball,
                            "file" => Unpack::File,
                            other => bail!("unknown unpack '{other}' (expected tarball|file)"),
                        });
                    },
                    Long("submodules") => submodules = true,
                    Long("dir") => {
                        dir = Some(
                            parser
                                .value()?
                                .string()
                                .map_err(|_| anyhow::anyhow!("bad dir"))?,
                        );
                    },
                    Long("follows") => {
                        let string = parser
                            .value()?
                            .string()
                            .map_err(|_| anyhow::anyhow!("bad follows"))?;
                        follows.push(parse_follows(&string));
                    },
                    Value(value) => {
                        let str = value
                            .string()
                            .map_err(|_| anyhow::anyhow!("bad argument"))?;
                        if name.is_none() {
                            name = Some(str);
                        } else if url.is_none() {
                            url = Some(str);
                        } else {
                            bail!("add takes at most <name> <url>");
                        }
                    },
                    Short(_) | Long(_) => return Err(arg.unexpected().into()),
                }
            }
            Ok(Command::Add {
                name: name.context("add: missing <name>")?,
                url: url.context("add: missing <url>")?,
                pin_type,
                unpack,
                dir,
                submodules,
                follows,
            })
        },
        "rm" => {
            let mut name = None;
            while let Some(arg) = parser.next()? {
                match arg {
                    Value(value) if name.is_none() => {
                        name = Some(value.string().map_err(|_| anyhow::anyhow!("bad name"))?);
                    },
                    Short(_) | Long(_) | Value(_) => return Err(arg.unexpected().into()),
                }
            }
            Ok(Command::Rm {
                name: name.context("rm: missing <name>")?,
            })
        },
        "alias" => {
            let mut rm = false;
            let (mut name_arg, mut template) = (None, None);
            while let Some(arg) = parser.next()? {
                match arg {
                    Long("rm") => rm = true,
                    Value(value) => {
                        let str = value
                            .string()
                            .map_err(|_| anyhow::anyhow!("bad argument"))?;
                        if name_arg.is_none() {
                            name_arg = Some(str);
                        } else if template.is_none() {
                            template = Some(str);
                        } else {
                            bail!("alias takes at most <name> <template>");
                        }
                    },
                    Short(_) | Long(_) => return Err(arg.unexpected().into()),
                }
            }
            let name = name_arg.context("alias: missing <name>")?;
            if !rm && template.is_none() {
                bail!("alias: missing <template> (or pass --rm)");
            }
            Ok(Command::Alias { name, template, rm })
        },
        "dedup" => {
            let mut deep = false;
            while let Some(arg) = parser.next()? {
                match arg {
                    Long("deep") => deep = true,
                    Short(_) | Long(_) | Value(_) => return Err(arg.unexpected().into()),
                }
            }
            Ok(Command::Dedup { deep })
        },
        _ => Ok(Command::Help),
    }
}

/// child=parent, or bare child -> follows the same-named pin.
fn parse_follows(str: &str) -> (String, String) {
    match str.split_once('=') {
        Some((child, parent)) => (child.to_owned(), parent.to_owned()),
        None => (str.to_owned(), str.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Command,
        parse_parser,
    };

    fn parse(args: &[&str]) -> Command {
        parse_parser(lexopt::Parser::from_args(args)).expect("arguments should parse")
    }

    #[test]
    fn help_aliases_parse_as_help() {
        assert_eq!(parse(&[]), Command::Help);
        assert_eq!(parse(&["help"]), Command::Help);
        assert_eq!(parse(&["-h"]), Command::Help);
        assert_eq!(parse(&["--help"]), Command::Help);
    }
}
