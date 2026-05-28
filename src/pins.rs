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

pub struct Input {
    pub name:       String,
    pub url:        String,
    pub submodules: bool,
}

pub fn load(path: &Path) -> Result<DocumentMut> {
    if !path.exists() {
        bail!("no pins.toml at {} (run `tack init`)", path.display());
    }
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    raw.parse()
        .with_context(|| format!("parse {}", path.display()))
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
        out.push(Input {
            name:       name.to_owned(),
            url:        url.to_owned(),
            submodules: entry
                .get("submodules")
                .and_then(Item::as_bool)
                .unwrap_or(false),
        });
    }
    Ok(out)
}

pub fn has_input(doc: &DocumentMut, name: &str) -> bool {
    doc.get("inputs")
        .and_then(Item::as_table)
        .is_some_and(|tbl| tbl.contains_key(name))
}

pub fn add_input(
    doc: &mut DocumentMut,
    name: &str,
    url: &str,
    flake: bool,
    dir: Option<&str>,
    submodules: bool,
    follows: &[(String, String)],
) {
    let mut entry = Table::new();
    entry.set_implicit(false);
    entry["url"] = value(url);
    if !flake {
        entry["flake"] = value(false);
    }
    if let Some(subdir) = dir {
        entry["dir"] = value(subdir);
    }
    if submodules {
        entry["submodules"] = value(true);
    }
    if !follows.is_empty() {
        let mut follows_tbl = Table::new();
        for &(ref child, ref parent) in follows {
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
