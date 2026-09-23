// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    fmt::{
        Display,
        Formatter,
        Result as FmtResult,
    },
    path::Path,
    str::FromStr,
};

use misstep::{
    OptionExt as _,
    Result,
    ResultExt as _,
};
use pound::ValueEnum;
use toml_edit::{
    Array,
    DocumentMut,
    Item,
    Table,
    value,
};

use crate::{
    error::user_bail,
    lock::DECLARED_KEY,
    patched::source::PatchSource,
    project::write_atomic,
    shorturl::ShortUrls,
    signers::{
        Keyring,
        SignerKey,
        SignerName,
        key_file,
    },
    tag::{
        self,
        TagTemplate,
    },
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
}

impl Display for PinType {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.as_str())
    }
}

impl FromStr for PinType {
    type Err = misstep::Report;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "flake" => Ok(Self::Flake),
            "fetch" => Ok(Self::Fetch),
            "fixed" => Ok(Self::Fixed),
            other => user_bail!("unknown pin type '{other}' (expected flake|fetch|fixed)"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
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

    pub fn detect(url: &str) -> Self {
        let no_query = url.split('?').next().unwrap_or(url);
        let path = no_query.split('#').next().unwrap_or(no_query);
        let lower = path.to_ascii_lowercase();
        let tarballish = [
            ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz", ".tbz2", ".tar.xz", ".txz", ".tar.zst",
            ".tzst",
        ];
        if tarballish.iter().any(|ending| lower.ends_with(ending)) {
            Self::Tarball
        } else {
            Self::File
        }
    }
}

impl Display for Unpack {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.as_str())
    }
}

impl FromStr for Unpack {
    type Err = misstep::Report;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "tarball" => Ok(Self::Tarball),
            "file" => Ok(Self::File),
            other => user_bail!("unknown unpack '{other}' (expected tarball|file)"),
        }
    }
}

#[derive(Debug)]
pub struct Input {
    pub name:       String,
    pub url:        String,
    pub submodules: bool,
    pub pin_type:   PinType,
    pub unpack:     Option<Unpack>,
    pub dir:        Option<String>,
    pub follows:    BTreeMap<String, String>,
    pub excludes:   BTreeSet<String>,
    pub signers:    Vec<SignerName>,
    pub patches:    Vec<PatchSource>,
    pub tag:        Option<TagTemplate>,
    pub group:      Option<String>,
    pub frozen:     bool,
}

impl Input {
    fn from_item(name: &str, input_item: &Item, shorturls: &ShortUrls<'_>) -> Result<Self> {
        if name == DECLARED_KEY {
            user_bail!("'{name}' is reserved for the lock file");
        }
        let entry = input_item
            .as_table_like()
            .with_context(|| format!("input '{name}' is not a table"))?;
        let url = entry
            .get("url")
            .and_then(Item::as_str)
            .with_context(|| format!("input '{name}' has no url"))?;
        let str_field = |key: &str| {
            entry
                .get(key)
                .map(|item| {
                    item.as_str()
                        .with_context(|| format!("input '{name}': {key} must be a string"))
                })
                .transpose()
        };
        let bool_field = |key: &str| {
            entry
                .get(key)
                .map(|item| {
                    item.as_bool()
                        .with_context(|| format!("input '{name}': {key} must be a bool"))
                })
                .transpose()
        };
        let pin_type = match str_field("type")? {
            Some(typ) => {
                typ.parse::<PinType>()
                    .with_context(|| format!("input '{name}'"))?
            },
            None if bool_field("flake")? == Some(false) => PinType::Fetch,
            None => PinType::Flake,
        };
        let unpack = str_field("unpack")?
            .map(|unpack| {
                unpack
                    .parse::<Unpack>()
                    .with_context(|| format!("input '{name}'"))
            })
            .transpose()?;
        if pin_type != PinType::Fixed && unpack.is_some() {
            user_bail!("input '{name}': unpack is only valid for type = \"fixed\"");
        }
        let follows = follows_table(name, entry.get("follows"))?;
        let excludes = string_array(name, "exclude_follow", entry.get("exclude_follow"))?
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let dir = str_field("dir")?;
        let signers = string_array(name, "signers", entry.get("signers"))?
            .into_iter()
            .map(str::parse::<SignerName>)
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("input '{name}'"))?;
        if pin_type == PinType::Fixed && !signers.is_empty() {
            user_bail!("input '{name}': signers are not valid for type = \"fixed\"");
        }
        let patches = string_array(name, "patches", entry.get("patches"))?
            .into_iter()
            .map(|raw| {
                PatchSource::parse(raw, shorturls).with_context(|| format!("input '{name}'"))
            })
            .collect::<Result<Vec<_>>>()?;
        if pin_type == PinType::Fixed && !patches.is_empty() {
            user_bail!("input '{name}': patches are not valid for type = \"fixed\"");
        }
        let tag = str_field("tag")?
            .map(|tag| {
                tag.parse::<TagTemplate>()
                    .with_context(|| format!("input '{name}'"))
            })
            .transpose()?;
        if pin_type == PinType::Fixed && tag.is_some() {
            user_bail!("input '{name}': tag is not valid for type = \"fixed\"");
        }
        if tag.is_some() {
            tag::followable(name, &shorturls.expand(url)?)?;
        }
        let group = str_field("group")?;
        let frozen = bool_field("frozen")?.unwrap_or(false);
        let submodules = bool_field("submodules")?.unwrap_or(false);
        Ok(Self {
            name: name.to_owned(),
            url: url.to_owned(),
            submodules,
            pin_type,
            unpack,
            dir: dir.map(str::to_owned),
            follows,
            excludes,
            signers,
            patches,
            tag,
            group: group.map(str::to_owned),
            frozen,
        })
    }
}

fn follows_table(name: &str, item: Option<&Item>) -> Result<BTreeMap<String, String>> {
    let Some(follows_item) = item else {
        return Ok(BTreeMap::new());
    };
    let tbl = follows_item
        .as_table_like()
        .with_context(|| format!("input '{name}': follows must be a table"))?;
    tbl.iter()
        .map(|(child, target_item)| {
            let target = target_item
                .as_str()
                .with_context(|| format!("input '{name}': follows.{child} must be a string"))?;
            Ok((child.to_owned(), target.to_owned()))
        })
        .collect()
}

fn string_array<'item>(
    name: &str,
    key: &str,
    item: Option<&'item Item>,
) -> Result<Vec<&'item str>> {
    let Some(array_item) = item else {
        return Ok(Vec::new());
    };
    let arr = array_item
        .as_array()
        .with_context(|| format!("input '{name}': {key} must be an array of strings"))?;
    arr.iter()
        .enumerate()
        .map(|(index, member)| {
            member
                .as_str()
                .with_context(|| format!("input '{name}': {key}[{index}] must be a string"))
        })
        .collect::<Result<Vec<_>>>()
}

fn signer_values(item: &Item) -> Option<Vec<&str>> {
    if let Some(value) = item.as_str() {
        return Some(vec![value]);
    }
    item.as_array()?
        .iter()
        .map(toml_edit::Value::as_str)
        .collect()
}

#[derive(Debug)]
pub struct PinsDoc {
    doc: DocumentMut,
}

impl PinsDoc {
    pub fn parse(raw: &str) -> Result<Self, toml_edit::TomlError> {
        raw.parse().map(|doc| Self { doc })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, self.doc.to_string())
    }

    pub fn shorturls(&self) -> ShortUrls<'_> {
        let mut templates = BTreeMap::new();
        if let Some(table) = self.doc.get("shorturls").and_then(Item::as_table) {
            for (key, value) in table {
                if let Some(val) = value.as_str() {
                    templates.insert(key, val);
                }
            }
        }
        ShortUrls::new(templates)
    }

    pub fn all_follows(&self) -> Result<BTreeMap<String, String>> {
        AllFollowTable::from_doc(&self.doc).aliases()
    }

    pub fn inputs(&self) -> Result<Vec<Input>> {
        let mut out = Vec::new();
        let Some(table) = self.doc.get("inputs").and_then(Item::as_table_like) else {
            return Ok(out);
        };
        let shorturls = self.shorturls();
        for (name, item) in table.iter() {
            out.push(Input::from_item(name, item, &shorturls)?);
        }
        self.signers()?;
        let declared = self.doc.get("signers").and_then(Item::as_table_like);
        for input in &out {
            if let Some(signer) = input.signers.iter().find(|signer| {
                !declared.is_some_and(|signers| signers.contains_key(signer.as_str()))
            }) {
                user_bail!("input '{}': no signer '{signer}' in [signers]", input.name);
            }
            if let Some(ref group) = input.group
                && out.iter().any(|other| other.name == *group)
            {
                user_bail!(
                    "input '{}' is in group '{group}', which is also an input name",
                    input.name
                );
            }
        }
        Ok(out)
    }

    /// the keys of every signer that one of `inputs` lists
    pub fn keyring(&self, inputs: &[&Input], dir: &Path) -> Result<Keyring> {
        let listed = self
            .signers()?
            .into_iter()
            .filter(|&(ref name, _)| inputs.iter().any(|input| input.signers.contains(name)))
            .collect();
        Keyring::load(listed, dir)
    }

    /// each signer's values, every one a key or a path under `.tack` to one
    pub fn signers(&self) -> Result<Vec<(SignerName, Vec<&str>)>> {
        let Some(table) = self.doc.get("signers").and_then(Item::as_table_like) else {
            return Ok(Vec::new());
        };
        table
            .iter()
            .map(|(name, item)| {
                let signer = name.parse::<SignerName>()?;
                let values = signer_values(item).with_context(|| {
                    format!("signer '{signer}' must be a string or an array of strings")
                })?;
                for value in &values {
                    if value.parse::<SignerKey>().is_err() {
                        key_file(value).with_context(|| format!("signer '{signer}'"))?;
                    }
                }
                Ok((signer, values))
            })
            .collect::<Result<Vec<_>>>()
    }

    pub fn has_signer(&self, name: &SignerName) -> bool {
        self.doc
            .get("signers")
            .and_then(Item::as_table_like)
            .is_some_and(|signers| signers.contains_key(name.as_str()))
    }

    /// files under `.tack` that the signer's value names
    pub fn signer_files(&self, name: &SignerName) -> Vec<String> {
        self.doc
            .get("signers")
            .and_then(Item::as_table_like)
            .and_then(|signers| signers.get(name.as_str()))
            .and_then(signer_values)
            .unwrap_or_default()
            .into_iter()
            .filter(|value| value.parse::<SignerKey>().is_err())
            .map(str::to_owned)
            .collect()
    }

    pub fn add_signer(&mut self, name: &SignerName, values: &[&str]) {
        let item = match *values {
            [single] => value(single),
            _ => value(values.iter().copied().collect::<Array>()),
        };
        self.ensure_table("signers").insert(name.as_str(), item);
    }

    pub fn remove_signer(&mut self, name: &SignerName) -> bool {
        self.doc
            .get_mut("signers")
            .and_then(Item::as_table_like_mut)
            .and_then(|signers| signers.remove(name.as_str()))
            .is_some()
    }

    pub fn has_input(&self, name: &str) -> bool {
        self.doc
            .get("inputs")
            .and_then(Item::as_table_like)
            .is_some_and(|tbl| tbl.contains_key(name))
    }

    pub fn add_input(&mut self, name: &str, url: &str, opts: &AddInputOpts<'_>) {
        self.ensure_table("inputs")
            .insert(name, Item::Table(opts.to_table(url)));
    }

    pub fn remove_input(&mut self, name: &str) -> bool {
        self.doc
            .get_mut("inputs")
            .and_then(Item::as_table_like_mut)
            .and_then(|tbl| tbl.remove(name))
            .is_some()
    }

    /// # Panics
    ///
    /// if `name` isn't an input table, so pass names from `inputs`
    pub fn set_frozen(&mut self, name: &str, frozen: bool) {
        let entry = self
            .doc
            .get_mut("inputs")
            .and_then(|inputs| inputs.get_mut(name))
            .and_then(Item::as_table_like_mut)
            .expect("input listed by inputs()");
        if frozen {
            entry.insert("frozen", value(true));
        } else {
            entry.remove("frozen");
        }
    }

    pub fn add_patch(&mut self, name: &str, patch: &str) -> Result<()> {
        let item = self.input_item_mut(name)?;
        let entry = item
            .as_table_like_mut()
            .with_context(|| format!("input '{name}' is not a table"))?;
        match entry.get_mut("patches").and_then(Item::as_array_mut) {
            Some(patches) => {
                let multiline = patches.iter().any(|listed| {
                    listed
                        .decor()
                        .prefix()
                        .and_then(|prefix| prefix.as_str())
                        .is_some_and(|prefix| prefix.contains('\n'))
                });
                // a multiline array keeps its layout when the new entry takes the
                // indent of the last one, without the comments above it
                let indent = patches
                    .iter()
                    .last()
                    .and_then(|last| last.decor().prefix()?.as_str())
                    .and_then(|prefix| prefix.rsplit_once('\n'))
                    .map(|(_, indent)| format!("\n{indent}"));
                patches.push(patch);
                match (multiline, indent, patches.iter_mut().last()) {
                    (true, Some(prefix), Some(added)) => added.decor_mut().set_prefix(prefix),
                    (false, ..) => patches.fmt(),
                    (true, ..) => {},
                }
            },
            None => {
                entry.insert("patches", value(Array::from_iter([patch])));
            },
        }
        if let Some(inline) = item.as_inline_table_mut() {
            inline.fmt();
        }
        Ok(())
    }

    pub fn remove_patch(&mut self, name: &str, patch: &str) -> Result<()> {
        let item = self.input_item_mut(name)?;
        let entry = item
            .as_table_like_mut()
            .with_context(|| format!("input '{name}' is not a table"))?;
        let Some(patches) = entry.get_mut("patches").and_then(Item::as_array_mut) else {
            return Ok(());
        };
        let Some(index) = patches
            .iter()
            .position(|listed| listed.as_str() == Some(patch))
        else {
            return Ok(());
        };
        let removed = patches.remove(index);
        if patches.is_empty() {
            entry.remove("patches");
        } else if index == 0
            && let Some(first) = patches.get_mut(0)
        {
            let prefix = removed.decor().prefix().cloned().unwrap_or_default();
            first.decor_mut().set_prefix(prefix);
        }
        if let Some(inline) = item.as_inline_table_mut() {
            inline.fmt();
        }
        Ok(())
    }

    fn input_item_mut(&mut self, name: &str) -> Result<&mut Item> {
        self.doc
            .get_mut("inputs")
            .and_then(Item::as_table_like_mut)
            .and_then(|tbl| tbl.get_mut(name))
            .with_context(|| format!("no input '{name}'"))
    }

    pub fn set_alias(&mut self, name: &str, template: &str) {
        self.ensure_table("shorturls").insert(name, value(template));
    }

    pub fn remove_alias(&mut self, name: &str) -> bool {
        self.doc
            .get_mut("shorturls")
            .and_then(Item::as_table_mut)
            .and_then(|tbl| tbl.remove(name))
            .is_some()
    }

    pub fn mark_recomposable(&mut self) {
        self.ensure_table("tack")
            .insert("recomposable", value(true));
    }

    fn ensure_table(&mut self, name: &str) -> &mut Table {
        let table = self
            .doc
            .remove(name)
            .and_then(|item| item.into_table().ok())
            .unwrap_or_default();
        self.doc
            .entry(name)
            .or_insert(Item::Table(table))
            .as_table_mut()
            .expect("table was just inserted")
    }
}

/// `[all_follow]` flattened to child -> target
struct AllFollowTable<'a> {
    item: Option<&'a Item>,
}

impl<'a> AllFollowTable<'a> {
    fn from_doc(doc: &'a DocumentMut) -> Self {
        Self {
            item: doc.get("all_follow"),
        }
    }

    fn aliases(&self) -> Result<BTreeMap<String, String>> {
        let Some(item) = self.item else {
            return Ok(BTreeMap::new());
        };
        let table = item
            .as_table_like()
            .with_context(|| "all_follow must be a table")?;
        let mut out = BTreeMap::new();
        for (key, value) in table.iter() {
            if let Some(target) = value.as_str() {
                out.insert(key.to_owned(), target.to_owned());
            } else if let Some(arr) = value.as_array() {
                // array form uses the key as the target
                out.insert(key.to_owned(), key.to_owned());
                for (index, el) in arr.iter().enumerate() {
                    let alias = el
                        .as_str()
                        .with_context(|| format!("all_follow.{key}[{index}] must be a string"))?;
                    out.insert(alias.to_owned(), key.to_owned());
                }
            } else {
                user_bail!("all_follow.{key} must be a string or array of strings");
            }
        }
        Ok(out)
    }
}

#[derive(Clone, Copy)]
pub struct FollowAlias<'a> {
    raw: &'a str,
}

impl<'a> From<&'a str> for FollowAlias<'a> {
    fn from(raw: &'a str) -> Self {
        Self { raw }
    }
}

impl<'a> FollowAlias<'a> {
    pub fn flake_side(self) -> Option<&'a str> {
        match self.raw.split_once(':') {
            Some(("flake", rest)) => Some(rest),
            Some(("tack", _)) => None,
            _ => Some(self.raw),
        }
    }
}

pub struct AddInputOpts<'a> {
    pub pin_type:   PinType,
    pub unpack:     Option<Unpack>,
    pub dir:        Option<&'a str>,
    pub submodules: bool,
    pub follows:    &'a [(String, String)],
    pub tag:        Option<&'a TagTemplate>,
}

impl AddInputOpts<'_> {
    fn to_table(&self, url: &str) -> Table {
        let mut entry = Table::new();
        entry.set_implicit(false);
        entry.insert("url", value(url));
        if self.pin_type != PinType::Flake {
            entry.insert("type", value(self.pin_type.as_str()));
        }
        if let Some(unpak) = self.unpack {
            entry.insert("unpack", value(unpak.as_str()));
        }
        if let Some(subdir) = self.dir {
            entry.insert("dir", value(subdir));
        }
        if self.submodules {
            entry.insert("submodules", value(true));
        }
        if let Some(template) = self.tag {
            entry.insert("tag", value(template.to_string()));
        }
        if !self.follows.is_empty() {
            let mut follows_tbl = Table::new();
            for &(ref child, ref parent) in self.follows {
                follows_tbl.insert(child, value(parent.as_str()));
            }
            entry.insert("follows", Item::Table(follows_tbl));
        }
        entry
    }
}

#[cfg(test)]
#[path = "pins_tests.rs"]
mod tests;
