// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::BTreeSet,
    fs,
    result::Result as StdResult,
};

use misstep::Result;

use crate::{
    cli::{
        AddArgs,
        PatchAction,
        SignerAction,
    },
    fetch::FetchError,
    history::View,
    pins,
    project::Project,
    report::{
        DedupReport,
        LookReport,
        TreeReport,
        UpdateReport,
        VerifyReport,
    },
};

const STARTER_TOML: &str = include_str!("../../assets/pins.toml");
const RESOLVER_NIX: &str = include_str!("../../.tack/default.nix");
const SCAFFOLD_FLAKE: &str = include_str!("../../templates/default/flake.nix");
const MARKER: &str = "# tack-managed resolver.";

pub fn warn_stale_resolver(project: &Project) {
    if !stale_resolver(project) {
        return;
    }
    let path = project.resolver_path();
    eprintln!(
        "tack: resolver at {} is out of date. run `tack init --resolver` to update",
        path.display()
    );
}

pub fn stale_resolver(project: &Project) -> bool {
    fs::read_to_string(project.resolver_path())
        .is_ok_and(|current| current.contains(MARKER) && current != RESOLVER_NIX)
}

mod convert;
mod dedup;
mod edit;
mod init;
mod patch;
mod signer;
mod tree;
mod undo;
mod update;
mod verify;

#[derive(Clone, Copy)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "four orthogonal init switches, mapped straight from argv"
)]
pub struct InitRequest {
    pub force:    bool,
    pub resolver: bool,
    pub flake:    bool,
    pub convert:  bool,
}

pub fn init(project: &Project, request: InitRequest) -> Result<()> {
    init::init(project, request)
}

pub fn add(project: &Project, args: &AddArgs) -> Result<()> {
    edit::add(project, args)
}

pub fn rm(project: &Project, name: &str) -> Result<()> {
    edit::rm(project, name)
}

pub fn alias(project: &Project, name: &str, template: Option<&str>, remove: bool) -> Result<()> {
    edit::alias(project, name, template, remove)
}

pub fn set_frozen(project: &Project, names: &[String], frozen: bool) -> Result<()> {
    edit::set_frozen(project, names, frozen)
}

pub fn signer(project: &Project, action: &SignerAction) -> Result<()> {
    signer::run(project, action)
}

pub fn patch(project: &Project, action: &PatchAction) -> Result<()> {
    patch::run(project, action)
}

pub fn materialize(project: &Project, names: &[String]) -> Result<()> {
    patch::materialize(project, names)
}

pub fn update(project: &Project, selection: Selection<'_>, accept: bool) -> Result<UpdateReport> {
    update::update(project, selection, accept)
}

pub fn look(project: &Project, selection: Selection<'_>, verbose: bool) -> Result<LookReport> {
    update::look(project, selection, verbose)
}

pub fn update_cli(project: &Project, selection: Selection<'_>, accept: bool) -> Result<()> {
    update::update_cli(project, selection, accept)
}

pub fn look_cli(project: &Project, selection: Selection<'_>, verbose: bool) -> Result<()> {
    update::look_cli(project, selection, verbose)
}

pub fn tree(project: &Project, selection: Selection<'_>) -> Result<TreeReport> {
    tree::tree(project, selection)
}

pub fn tree_cli(project: &Project, selection: Selection<'_>) -> Result<()> {
    tree::tree_cli(project, selection)
}

pub fn verify(project: &Project, base: Option<&str>) -> Result<VerifyReport> {
    verify::verify(project, base)
}

pub fn verify_cli(project: &Project, base: Option<&str>) -> Result<()> {
    verify::verify_cli(project, base)
}

pub fn dedup(project: &Project) -> Result<()> {
    dedup::dedup(project)
}

pub fn dedup_report(project: &Project) -> Result<DedupReport> {
    dedup::dedup_report(project)
}

pub fn undo(project: &Project, list: bool) -> Result<()> {
    undo::undo(project, list)
}

pub fn redo(project: &Project) -> Result<()> {
    undo::redo(project)
}

pub fn history(project: &Project) -> Option<View> {
    undo::history(project)
}

pub fn undo_view(project: &Project) -> Result<Option<View>> {
    undo::undo_view(project)
}

pub fn redo_view(project: &Project) -> Result<Option<View>> {
    undo::redo_view(project)
}

fn tolerate<T>(result: StdResult<T, FetchError>) -> (Option<T>, Option<String>) {
    match result {
        Ok(value) => (Some(value), None),
        Err(FetchError::NotFound { .. }) => (None, None),
        Err(err) => (None, Some(err.to_string())),
    }
}

/// which pins a command should act on, before resolving against pins.toml
#[derive(Clone, Copy)]
pub struct Selection<'a> {
    pub names:   &'a [String],
    pub exclude: &'a [String],
}

impl<'a> Selection<'a> {
    pub const fn new(names: &'a [String], exclude: &'a [String]) -> Self {
        Self { names, exclude }
    }

    /// true when the command asked for every pin, so an empty result means an
    /// empty project
    pub const fn is_everything(&self) -> bool {
        self.names.is_empty() && self.exclude.is_empty()
    }
}

/// clusters pins by `group` in first-seen order, ungrouped pins last, so each
/// group header in `look` and `update` prints once
fn select<'a>(inputs: &'a [pins::Input], selection: Selection<'_>) -> Vec<&'a pins::Input> {
    let mut out = pick(inputs, selection);
    let mut groups = Vec::new();
    for group in out.iter().filter_map(|input| input.group.as_deref()) {
        if !groups.contains(&group) {
            groups.push(group);
        }
    }
    out.sort_by_key(|input| {
        input
            .group
            .as_deref()
            .and_then(|group| groups.iter().position(|seen| *seen == group))
            .unwrap_or(groups.len())
    });
    out
}

/// a name selects the input it names or every member of the group it names,
/// which [`PinsDoc::inputs`](crate::PinsDoc::inputs) keeps from overlapping
fn pick<'a>(inputs: &'a [pins::Input], selection: Selection<'_>) -> Vec<&'a pins::Input> {
    let Selection { names, exclude } = selection;
    let members = |name: &str| {
        inputs
            .iter()
            .filter(|input| input.name == name || input.group.as_deref() == Some(name))
            .collect::<Vec<_>>()
    };

    let mut excluded = BTreeSet::new();
    for name in exclude {
        let matched = members(name);
        if matched.is_empty() {
            eprintln!("tack: no input or group '{name}' to exclude");
        }
        excluded.extend(matched.into_iter().map(|input| input.name.as_str()));
    }

    if names.is_empty() {
        return inputs
            .iter()
            .filter(|input| !excluded.contains(input.name.as_str()))
            .collect();
    }

    let mut out = Vec::<&pins::Input>::new();
    for name in names {
        let matched = members(name);
        if matched.is_empty() {
            eprintln!("tack: no input or group '{name}'");
        } else if excluded.contains(name.as_str()) {
            eprintln!("tack: input '{name}' is both named and excluded, leaving it alone");
        } else {
            for input in matched {
                if !excluded.contains(input.name.as_str())
                    && !out.iter().any(|seen| seen.name == input.name)
                {
                    out.push(input);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::{
        BTreeMap,
        BTreeSet,
    };

    use super::*;
    use crate::pins::PinType;

    fn inputs(names: &[&str]) -> Vec<pins::Input> {
        names
            .iter()
            .map(|name| {
                pins::Input {
                    name:       (*name).to_owned(),
                    url:        format!("github:owner/{name}"),
                    submodules: false,
                    pin_type:   PinType::Flake,
                    unpack:     None,
                    dir:        None,
                    follows:    BTreeMap::new(),
                    excludes:   BTreeSet::new(),
                    signers:    Vec::new(),
                    patches:    Vec::new(),
                    group:      None,
                    frozen:     false,
                }
            })
            .collect()
    }

    fn selected(inputs: &[pins::Input], pick: &[&str], skip: &[&str]) -> Vec<String> {
        let names = pick.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        let exclude = skip.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        select(inputs, Selection {
            names:   &names,
            exclude: &exclude,
        })
        .iter()
        .map(|input| input.name.clone())
        .collect()
    }

    #[test]
    fn exclude_drops_pins_from_an_unnamed_update() {
        let all = inputs(&["nixpkgs", "home-manager", "nixvim"]);
        assert_eq!(selected(&all, &[], &["home-manager"]), [
            "nixpkgs", "nixvim"
        ]);
    }

    #[test]
    fn an_unknown_exclude_leaves_every_pin_selected() {
        let all = inputs(&["nixpkgs", "home-manager"]);
        assert_eq!(selected(&all, &[], &["nixpgks"]), [
            "nixpkgs",
            "home-manager"
        ]);
    }

    #[test]
    fn exclude_outranks_a_pin_named_on_the_same_run() {
        let all = inputs(&["nixpkgs", "home-manager"]);
        assert!(selected(&all, &["nixpkgs"], &["nixpkgs"]).is_empty());
        assert_eq!(
            selected(&all, &["nixpkgs", "home-manager"], &["nixpkgs"]),
            ["home-manager"]
        );
    }
}
