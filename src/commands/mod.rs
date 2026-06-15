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

pub fn update(project: &Project, names: &[String], accept: bool) -> Result<UpdateReport> {
    update::update(project, names, accept)
}

pub fn look(project: &Project, names: &[String], verbose: bool) -> Result<LookReport> {
    update::look(project, names, verbose)
}

pub fn update_cli(project: &Project, names: &[String], accept: bool) -> Result<()> {
    update::update_cli(project, names, accept)
}

pub fn look_cli(project: &Project, names: &[String], verbose: bool) -> Result<()> {
    update::look_cli(project, names, verbose)
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

fn select<'a>(inputs: &'a [pins::Input], names: &[String]) -> Vec<&'a pins::Input> {
    if names.is_empty() {
        return inputs.iter().collect();
    }
    let mut out = Vec::new();
    for n in names {
        match inputs.iter().find(|i| &i.name == n) {
            Some(i) => out.push(i),
            None => eprintln!("no input '{n}'"),
        }
    }
    out
}
