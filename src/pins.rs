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

pub fn load(path: &Path) -> Result<DocumentMut> {
    let raw = fs::read_to_string(path).wrap_err_with(|| format!("read {}", path.display()))?;
    parse_doc(&raw).wrap_err_with(|| format!("parse {}", path.display()))
}

/// parse pins.toml from an in-memory string
pub fn parse_doc(raw: &str) -> Result<DocumentMut, toml_edit::TomlError> {
    raw.parse()
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

/// the global `[all_follow]` table flattened to child name -> target name
///
/// two value shapes are accepted under the same table:
/// * `alias = "target"` - `alias` follows `target`
/// * `target = [a, b, ...]` - `target`, `a`, `b`, ... all follow `target`,
///   useful when several transitive names share a single canonical target
pub fn all_follows(doc: &DocumentMut) -> BTreeMap<String, String> {
    let Some(table) = doc.get("all_follow").and_then(Item::as_table) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (key, value) in table {
        if let Some(target) = value.as_str() {
            out.insert(key.to_owned(), target.to_owned());
        } else if let Some(arr) = value.as_array() {
            // key is its own target, plus every array member follows it too
            out.insert(key.to_owned(), key.to_owned());
            for el in arr {
                if let Some(alias) = el.as_str() {
                    out.insert(alias.to_owned(), key.to_owned());
                }
            }
        }
    }
    out
}

/// project a follows alias onto the flake side, for the flake.lock walk in
/// dedup synthesis: `dep`/`flake:dep` -> `dep`, `tack:dep` -> dropped.
pub fn flake_side(key: &str) -> Option<&str> {
    match key.split_once(':') {
        Some(("flake", rest)) => Some(rest),
        Some(("tack", _)) => None,
        _ => Some(key),
    }
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
            bail!("input '{name}': `unpack` is only valid for type = \"fixed\"");
        }
        let follows = entry
            .get("follows")
            .and_then(Item::as_table_like)
            .map(|tbl| {
                tbl.iter()
                    .filter_map(|(child, target)| {
                        target
                            .as_str()
                            .map(|val| (child.to_owned(), val.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let excludes = entry
            .get("exclude_follow")
            .and_then(Item::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|excl| excl.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        out.push(Input {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        PinType,
        Unpack,
        all_follows,
        flake_side,
        inputs,
        parse_doc,
    };

    #[test]
    fn all_follows_string_form() {
        let doc = parse_doc("[all_follow]\nnixpkgs = \"nixpkgs\"\ncrane = \"my-crane\"\n")
            .expect("parse");
        let map = all_follows(&doc);
        assert_eq!(map.get("nixpkgs").map(String::as_str), Some("nixpkgs"));
        assert_eq!(map.get("crane").map(String::as_str), Some("my-crane"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn all_follows_array_form_implies_key_alias() {
        let doc = parse_doc("[all_follow]\ngit-hooks = [\"git-hooks-nix\"]\n").expect("parse");
        let map = all_follows(&doc);
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
        let doc = parse_doc(raw).expect("parse");
        let map = all_follows(&doc);
        assert_eq!(map.get("nixpkgs").map(String::as_str), Some("nixpkgs"));
        assert_eq!(map.get("xwl").map(String::as_str), Some("xwl"));
        assert_eq!(map.get("xwl-stable").map(String::as_str), Some("xwl"));
        assert_eq!(map.get("xwl-unstable").map(String::as_str), Some("xwl"));
    }

    #[test]
    fn all_follows_empty_array_is_self_map() {
        let doc = parse_doc("[all_follow]\nfoo = []\n").expect("parse");
        let map = all_follows(&doc);
        assert_eq!(map.get("foo").map(String::as_str), Some("foo"));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn all_follows_missing_table_is_empty() {
        let doc = parse_doc("[inputs]\n").expect("parse");
        assert!(all_follows(&doc).is_empty());
    }

    #[test]
    fn all_follows_keeps_scoped_keys_verbatim() {
        let raw =
            "[all_follow]\n\"flake:dep\" = \"replacement\"\n\"tack:other\" = \"x\"\nbare = \"y\"\n";
        let doc = parse_doc(raw).expect("parse");
        let map = all_follows(&doc);

        // scoping is preserved raw, but consumers project per side
        assert_eq!(
            map.get("flake:dep").map(String::as_str),
            Some("replacement")
        );
        assert_eq!(map.get("tack:other").map(String::as_str), Some("x"));
        assert_eq!(map.get("bare").map(String::as_str), Some("y"));
    }

    #[test]
    fn flake_side_projects_scope() {
        assert_eq!(flake_side("dep"), Some("dep"));
        assert_eq!(flake_side("flake:dep"), Some("dep"));
        assert_eq!(flake_side("tack:dep"), None);
    }

    #[test]
    fn inputs_read_type_unpack_and_legacy_flake_from_each_entry() {
        let doc = parse_doc(
            r#"
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
"#,
        )
        .expect("parse");

        let parsed = inputs(&doc).expect("inputs");
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
}
