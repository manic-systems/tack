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
    fs,
    path::Path,
    str::FromStr,
};

use eyre::{
    ContextCompat as _,
    Result,
    WrapErr as _,
    bail,
};
use toml_edit::{
    DocumentMut,
    Item,
    Table,
    value,
};

use crate::shorturl::ShortUrls;

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
    type Err = eyre::Report;

    fn from_str(s: &str) -> Result<Self> {
        match s {
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

    /// guess from a url extension, tarball-family wins, otherwise file
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

impl Display for Unpack {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.as_str())
    }
}

impl FromStr for Unpack {
    type Err = eyre::Report;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "tarball" => Ok(Self::Tarball),
            "file" => Ok(Self::File),
            other => bail!("unknown unpack '{other}' (expected tarball|file)"),
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
    /// per-pin `follows` from `[inputs.<name>.follows]`
    pub follows:    BTreeMap<String, String>,
    /// per-pin `exclude_follow` from `[inputs.<name>]`, names that opt out of
    /// the global `[all_follow]` rules
    pub excludes:   BTreeSet<String>,
}

impl Input {
    fn from_item(name: &str, input_item: &Item) -> Result<Self> {
        let entry = input_item
            .as_table_like()
            .with_context(|| format!("input '{name}' is not a table"))?;
        let url = entry
            .get("url")
            .and_then(Item::as_str)
            .with_context(|| format!("input '{name}' has no url"))?;
        let pin_type = match entry.get("type").and_then(Item::as_str) {
            Some(typ) => {
                typ.parse::<PinType>()
                    .wrap_err_with(|| format!("input '{name}'"))?
            },
            None => {
                match entry.get("flake").and_then(Item::as_bool) {
                    Some(false) => PinType::Fetch,
                    _ => PinType::Flake,
                }
            },
        };
        let unpack = entry
            .get("unpack")
            .and_then(Item::as_str)
            .map(|unpack| {
                unpack
                    .parse::<Unpack>()
                    .wrap_err_with(|| format!("input '{name}'"))
            })
            .transpose()?;
        if pin_type != PinType::Fixed && unpack.is_some() {
            bail!("input '{name}': unpack is only valid for type = \"fixed\"");
        }
        let follows = match entry.get("follows") {
            Some(follows_item) => {
                let tbl = follows_item
                    .as_table_like()
                    .with_context(|| format!("input '{name}': follows must be a table"))?;
                let mut follows = BTreeMap::new();
                for (child, target_item) in tbl.iter() {
                    let target = target_item.as_str().with_context(|| {
                        format!("input '{name}': follows.{child} must be a string")
                    })?;
                    follows.insert(child.to_owned(), target.to_owned());
                }
                follows
            },
            None => BTreeMap::new(),
        };
        let excludes = match entry.get("exclude_follow") {
            Some(exclude_item) => {
                let arr = exclude_item.as_array().with_context(|| {
                    format!("input '{name}': exclude_follow must be an array of strings")
                })?;
                let mut excludes = BTreeSet::new();
                for (index, exclude_member) in arr.iter().enumerate() {
                    let exclude = exclude_member.as_str().with_context(|| {
                        format!("input '{name}': exclude_follow[{index}] must be a string")
                    })?;
                    excludes.insert(exclude.to_owned());
                }
                excludes
            },
            None => BTreeSet::new(),
        };
        Ok(Self {
            name: name.to_owned(),
            url: url.to_owned(),
            submodules: entry
                .get("submodules")
                .and_then(Item::as_bool)
                .unwrap_or(false),
            pin_type,
            unpack,
            follows,
            excludes,
        })
    }
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
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, self.doc.to_string())?;
        fs::rename(&tmp, path)?;
        Ok(())
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
        let Some(table) = self.doc.get("inputs").and_then(Item::as_table) else {
            return Ok(out);
        };
        for (name, item) in table {
            out.push(Input::from_item(name, item)?);
        }
        Ok(out)
    }

    pub fn has_input(&self, name: &str) -> bool {
        self.doc
            .get("inputs")
            .and_then(Item::as_table)
            .is_some_and(|tbl| tbl.contains_key(name))
    }

    pub fn add_input(&mut self, name: &str, url: &str, opts: &AddInputOpts<'_>) {
        self.ensure_table("inputs")
            .insert(name, Item::Table(opts.to_table(url)));
    }

    pub fn remove_input(&mut self, name: &str) -> bool {
        self.doc
            .get_mut("inputs")
            .and_then(Item::as_table_mut)
            .and_then(|tbl| tbl.remove(name))
            .is_some()
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
        if self.doc.get(name).and_then(Item::as_table).is_none() {
            self.doc.insert(name, Item::Table(Table::new()));
        }
        self.doc
            .get_mut(name)
            .and_then(Item::as_table_mut)
            .expect("table was just inserted")
    }
}

/// the global `[all_follow]` table flattened to child name -> target name
///
/// two value shapes are accepted under the same table
/// * `alias = "target"` makes `alias` follow `target`
/// * `target = [a, b, ...]` makes every listed alias follow `target`
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
                // key is its own target, and every array member follows it too
                out.insert(key.to_owned(), key.to_owned());
                for (index, el) in arr.iter().enumerate() {
                    let alias = el
                        .as_str()
                        .with_context(|| format!("all_follow.{key}[{index}] must be a string"))?;
                    out.insert(alias.to_owned(), key.to_owned());
                }
            } else {
                bail!("all_follow.{key} must be a string or array of strings");
            }
        }
        Ok(out)
    }
}

/// project a follows alias onto the flake side for the flake.lock walk
/// `dep` and `flake:dep` are kept, `tack:dep` is dropped
#[derive(Clone, Copy)]
pub struct FollowAlias<'a> {
    raw: &'a str,
}

impl<'a> FollowAlias<'a> {
    pub const fn new(raw: &'a str) -> Self {
        Self { raw }
    }

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
mod tests {
    use std::collections::BTreeMap;

    use super::{
        FollowAlias,
        PinType,
        PinsDoc,
        Unpack,
    };

    fn doc(raw: &str) -> PinsDoc {
        PinsDoc::parse(raw).expect("parse")
    }

    #[test]
    fn all_follows_string_form() {
        let doc = doc("[all_follow]\nnixpkgs = \"nixpkgs\"\ncrane = \"my-crane\"\n");
        let map = doc.all_follows().unwrap();
        assert_eq!(map.get("nixpkgs").map(String::as_str), Some("nixpkgs"));
        assert_eq!(map.get("crane").map(String::as_str), Some("my-crane"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn all_follows_array_form_implies_key_alias() {
        let doc = doc("[all_follow]\ngit-hooks = [\"git-hooks-nix\"]\n");
        let map = doc.all_follows().unwrap();
        // both key and array members alias to key
        assert_eq!(map.get("git-hooks").map(String::as_str), Some("git-hooks"));
        assert_eq!(
            map.get("git-hooks-nix").map(String::as_str),
            Some("git-hooks")
        );
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn all_follows_mixed_forms_coexist() {
        let raw = "[all_follow]\nnixpkgs = \"nixpkgs\"\nxwl = [\"xwl-stable\", \"xwl-unstable\"]\n";
        let doc = doc(raw);
        let map = doc.all_follows().unwrap();
        assert_eq!(map.get("nixpkgs").map(String::as_str), Some("nixpkgs"));
        assert_eq!(map.get("xwl").map(String::as_str), Some("xwl"));
        assert_eq!(map.get("xwl-stable").map(String::as_str), Some("xwl"));
        assert_eq!(map.get("xwl-unstable").map(String::as_str), Some("xwl"));
    }

    #[test]
    fn all_follows_empty_array_is_self_map() {
        let doc = doc("[all_follow]\nfoo = []\n");
        let map = doc.all_follows().unwrap();
        assert_eq!(map.get("foo").map(String::as_str), Some("foo"));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn all_follows_missing_table_is_empty() {
        let doc = doc("[inputs]\n");
        assert!(doc.all_follows().unwrap().is_empty());
    }

    #[test]
    fn all_follows_keeps_scoped_keys_verbatim() {
        let raw =
            "[all_follow]\n\"flake:dep\" = \"replacement\"\n\"tack:other\" = \"x\"\nbare = \"y\"\n";
        let doc = doc(raw);
        let map = doc.all_follows().unwrap();

        // scoping is preserved raw, but consumers project per side
        assert_eq!(
            map.get("flake:dep").map(String::as_str),
            Some("replacement")
        );
        assert_eq!(map.get("tack:other").map(String::as_str), Some("x"));
        assert_eq!(map.get("bare").map(String::as_str), Some("y"));
    }

    #[test]
    fn all_follows_rejects_wrong_value_type() {
        let doc = doc("[all_follow]\nfoo = 3\n");
        let err = doc.all_follows().unwrap_err().to_string();
        assert!(err.contains("all_follow.foo must be a string or array of strings"));
    }

    #[test]
    fn all_follows_rejects_wrong_array_member_type() {
        let doc = doc("[all_follow]\nfoo = [\"bar\", 3]\n");
        let err = doc.all_follows().unwrap_err().to_string();
        assert!(err.contains("all_follow.foo[1] must be a string"));
    }

    #[test]
    fn flake_side_projects_scope() {
        assert_eq!(FollowAlias::new("dep").flake_side(), Some("dep"));
        assert_eq!(FollowAlias::new("flake:dep").flake_side(), Some("dep"));
        assert_eq!(FollowAlias::new("tack:dep").flake_side(), None);
    }

    #[test]
    fn inputs_read_type_unpack_and_legacy_flake_from_each_entry() {
        let doc = doc(r#"
[inputs.default]
url = "github:o/default"

[inputs.source]
url = "github:o/source"
type = "fetch"

[inputs.archive]
url = "https://example.com/archive.tar.gz"
type = "fixed"
unpack = "tarball"

[inputs.legacy]
url = "github:o/legacy"
flake = false
"#);

        let parsed = doc.inputs().expect("inputs");
        let by_name = parsed
            .iter()
            .map(|inp| (inp.name.as_str(), inp))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(by_name["default"].pin_type, PinType::Flake);
        assert_eq!(by_name["source"].pin_type, PinType::Fetch);
        assert_eq!(by_name["archive"].pin_type, PinType::Fixed);
        assert_eq!(by_name["archive"].unpack, Some(Unpack::Tarball));
        assert_eq!(by_name["legacy"].pin_type, PinType::Fetch);
    }

    #[test]
    fn inputs_reject_malformed_follows() {
        let doc = doc(r#"
[inputs.bad]
url = "github:o/r"

[inputs.bad.follows]
dep = 3
"#);
        let err = doc.inputs().unwrap_err().to_string();
        assert!(err.contains("input 'bad': follows.dep must be a string"));
    }

    #[test]
    fn inputs_reject_malformed_exclude_follow() {
        let doc = doc(r#"
[inputs.bad]
url = "github:o/r"
exclude_follow = ["dep", 3]
"#);
        let err = doc.inputs().unwrap_err().to_string();
        assert!(err.contains("input 'bad': exclude_follow[1] must be a string"));
    }
}
