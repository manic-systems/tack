// SPDX-License-Identifier: EUPL-1.2

use std::{
    fs,
    result::Result as StdResult,
};

use eyre::Result;

use crate::{
    fetch::FetchError,
    history::View,
    pins::{
        self,
        PinType,
        Unpack,
    },
    project::Project,
    report::{
        DedupReport,
        LookReport,
        UpdateReport,
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
mod undo;
mod update;

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

#[derive(Clone, Copy)]
pub struct AddRequest<'a> {
    pub name:       &'a str,
    pub url:        &'a str,
    pub pin_type:   PinType,
    pub unpack:     Option<Unpack>,
    pub dir:        Option<&'a str>,
    pub submodules: bool,
    pub follows:    &'a [(String, String)],
}

pub fn add(project: &Project, request: AddRequest<'_>) -> Result<()> {
    edit::add(project, request)
}

pub fn rm(project: &Project, name: &str) -> Result<()> {
    edit::rm(project, name)
}

pub fn alias(project: &Project, name: &str, template: Option<&str>, remove: bool) -> Result<()> {
    edit::alias(project, name, template, remove)
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
        Err(
            FetchError::NotFound { .. } | FetchError::Transport(_) | FetchError::RateLimited { .. },
        ) => (None, None),
        Err(err) => (None, Some(err.to_string())),
    }
}

/// which pins a command should act on, before resolving against pins.toml
#[derive(Clone, Copy)]
pub struct Selection<'a> {
    pub names:   &'a [String],
    pub exclude: &'a [String],
}

impl Selection<'_> {
    /// true when the command asked for every pin, so an empty result means an
    /// empty project
    pub const fn is_everything(&self) -> bool {
        self.names.is_empty() && self.exclude.is_empty()
    }
}

fn select<'a>(inputs: &'a [pins::Input], selection: Selection<'_>) -> Vec<&'a pins::Input> {
    let Selection { names, exclude } = selection;
    let known = |name: &String| inputs.iter().any(|input| input.name == *name);

    for name in exclude.iter().filter(|name| !known(name)) {
        eprintln!("tack: no input '{name}' to exclude");
    }

    if names.is_empty() {
        return inputs
            .iter()
            .filter(|input| !exclude.contains(&input.name))
            .collect();
    }

    let mut out = Vec::new();
    for name in names {
        if !known(name) {
            eprintln!("tack: no input '{name}'");
        } else if exclude.contains(name) {
            eprintln!("tack: input '{name}' is both named and excluded, leaving it alone");
        } else {
            out.extend(inputs.iter().find(|input| input.name == *name));
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
                    follows:    BTreeMap::new(),
                    excludes:   BTreeSet::new(),
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
