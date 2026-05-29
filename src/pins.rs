// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
};

use anyhow::{
    Context as _,
    Result,
    bail,
};
use toml_edit::{
    DocumentMut,
    Item,
    Table,
    value,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PinType {
    Flake,
    Fetch,
    Fixed,
}

impl PinType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Flake => "flake",
            Self::Fetch => "fetch",
            Self::Fixed => "fixed",
        }
    }

    fn parse(str: &str) -> Result<Self> {
        match str {
            "flake" => Ok(Self::Flake),
            "fetch" => Ok(Self::Fetch),
            "fixed" => Ok(Self::Fixed),
            other => bail!("unknown pin type '{other}' (expected flake|fetch|fixed)"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unpack {
    Tarball,
    File,
}

impl Unpack {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tarball => "tarball",
            Self::File => "file",
        }
    }

    fn parse(str: &str) -> Result<Self> {
        match str {
            "tarball" => Ok(Self::Tarball),
            "file" => Ok(Self::File),
            other => bail!("unknown unpack '{other}' (expected tarball|file)"),
        }
    }

    /// guess from a URL extension; tarball-family wins, otherwise file
    pub fn detect(url: &str) -> Self {
        let no_query = url.split('?').next().unwrap_or(url);
        let path = no_query.split('#').next().unwrap_or(no_query);
        let lower = path.to_ascii_lowercase();
        let tarballish = [
            ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz", ".tbz2", ".tar.xz", ".txz",
        ];
        if tarballish.iter().any(|ending| lower.ends_with(ending)) {
            Self::Tarball
        } else {
            Self::File
        }
    }
}

pub struct Input {
    pub name:       String,
    pub url:        String,
    pub submodules: bool,
    pub pin_type:   PinType,
    pub unpack:     Option<Unpack>,
}

pub fn load(path: &Path) -> Result<DocumentMut> {
    if !path.exists() {
        bail!("no pins.toml at {} (run `tack init`)", path.display());
    }
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    parse_doc(&raw).with_context(|| format!("parse {}", path.display()))
}

/// parse pins.toml from an in-memory string
pub fn parse_doc(raw: &str) -> Result<DocumentMut> {
    raw.parse().context("parse pins.toml")
}

pub fn save(path: &Path, doc: &DocumentMut) -> Result<()> {
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, doc.to_string())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn shorturls(doc: &DocumentMut) -> BTreeMap<&str, &str> {
    let mut out = BTreeMap::new();
    if let Some(table) = doc.get("shorturls").and_then(Item::as_table) {
        for (key, value) in table {
            if let Some(val) = value.as_str() {
                out.insert(key, val);
            }
        }
    }
    out
}

pub fn inputs(doc: &DocumentMut) -> Result<Vec<Input>> {
    let mut out = Vec::new();
    let Some(table) = doc.get("inputs").and_then(Item::as_table) else {
        return Ok(out);
    };
    for (name, item) in table {
        let entry = item
            .as_table_like()
            .with_context(|| format!("input '{name}' is not a table"))?;
        let url = entry
            .get("url")
            .and_then(Item::as_str)
            .with_context(|| format!("input '{name}' has no url"))?;
        // `type` is canonical; legacy `flake = false` reads as `fetch`
        let pin_type = match table.get("type").and_then(Item::as_str) {
            Some(typ) => PinType::parse(typ).with_context(|| format!("input '{name}'"))?,
            None => {
                match table.get("flake").and_then(Item::as_bool) {
                    Some(false) => PinType::Fetch,
                    _ => PinType::Flake,
                }
            },
        };
        let unpack = table
            .get("unpack")
            .and_then(Item::as_str)
            .map(|unpack| Unpack::parse(unpack).with_context(|| format!("input '{name}'")))
            .transpose()?;
        if pin_type != PinType::Fixed && unpack.is_some() {
            bail!("input '{name}': `unpack` is only valid for type = \"fixed\"");
        }
        out.push(Input {
            name: name.to_owned(),
            url: url.to_owned(),
            submodules: entry
                .get("submodules")
                .and_then(Item::as_bool)
                .unwrap_or(false),
            pin_type,
            unpack,
        });
    }
    Ok(out)
}

pub fn has_input(doc: &DocumentMut, name: &str) -> bool {
    doc.get("inputs")
        .and_then(Item::as_table)
        .is_some_and(|tbl| tbl.contains_key(name))
}

pub struct AddInputOpts<'a> {
    pub pin_type:   PinType,
    pub unpack:     Option<Unpack>,
    pub dir:        Option<&'a str>,
    pub submodules: bool,
    pub follows:    &'a [(String, String)],
}

pub fn add_input(doc: &mut DocumentMut, name: &str, url: &str, opts: &AddInputOpts<'_>) {
    let mut entry = Table::new();
    entry.set_implicit(false);
    entry["url"] = value(url);
    if opts.pin_type != PinType::Flake {
        entry["type"] = value(opts.pin_type.as_str());
    }
    if let Some(unpak) = opts.unpack {
        entry["unpack"] = value(unpak.as_str());
    }
    if let Some(subdir) = opts.dir {
        entry["dir"] = value(subdir);
    }
    if opts.submodules {
        entry["submodules"] = value(true);
    }
    if !opts.follows.is_empty() {
        let mut follows_tbl = Table::new();
        for &(ref child, ref parent) in opts.follows {
            follows_tbl[child] = value(parent.as_str());
        }
        entry["follows"] = Item::Table(follows_tbl);
    }
    if doc.get("inputs").and_then(Item::as_table).is_none() {
        doc["inputs"] = Item::Table(Table::new());
    }
    doc["inputs"][name] = Item::Table(entry);
}

pub fn remove_input(doc: &mut DocumentMut, name: &str) -> bool {
    doc.get_mut("inputs")
        .and_then(Item::as_table_mut)
        .and_then(|tbl| tbl.remove(name))
        .is_some()
}

pub fn set_alias(doc: &mut DocumentMut, name: &str, template: &str) {
    if doc.get("shorturls").and_then(Item::as_table).is_none() {
        doc["shorturls"] = Item::Table(Table::new());
    }
    doc["shorturls"][name] = value(template);
}

pub fn remove_alias(doc: &mut DocumentMut, name: &str) -> bool {
    doc.get_mut("shorturls")
        .and_then(Item::as_table_mut)
        .and_then(|tbl| tbl.remove(name))
        .is_some()
}
