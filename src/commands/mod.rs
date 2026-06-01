// SPDX-License-Identifier: EUPL-1.2

use std::{
    fs,
    result::Result as StdResult,
};

use eyre::Result;

use crate::{
    fetch::http::FetchError,
    pins::{
        self,
        PinType,
        Unpack,
    },
    project::Project,
};

const STARTER_TOML: &str = include_str!("../../assets/pins.toml");
const RESOLVER_NIX: &str = include_str!("../../.tack/default.nix");
const SCAFFOLD_FLAKE: &str = include_str!("../../templates/default/flake.nix");
const MARKER: &str = "# tack-managed resolver.";

/// warn when the resolver carries tack's marker but drifted from the template
/// silent for forked resolvers and uninitialized projects
pub fn warn_stale_resolver(project: &Project) {
    let path = project.resolver_path();
    if let Ok(current) = fs::read_to_string(&path)
        && current.contains(MARKER)
        && current != RESOLVER_NIX
    {
        eprintln!(
            "tack: resolver at {} is out of date. run `tack init --resolver` to update",
            path.display()
        );
    }
}

mod dedup;
mod edit;
mod init;
mod undo;
mod update;

pub fn init(project: &Project, force: bool, resolver_only: bool, flake: bool) -> Result<()> {
    init::init(project, force, resolver_only, flake)
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

pub fn update(project: &Project, names: &[String], accept: bool) -> Result<()> {
    update::update(project, names, accept)
}

pub fn look(project: &Project, names: &[String], verbose: bool) -> Result<()> {
    update::look(project, names, verbose)
}

pub fn dedup(project: &Project) -> Result<()> {
    dedup::dedup(project)
}

pub fn undo(project: &Project, list: bool) -> Result<()> {
    undo::undo(project, list)
}

pub fn redo(project: &Project) -> Result<()> {
    undo::redo(project)
}

pub fn help() {
    init::help();
}

/// disposition of a swallowed fetch result
/// expected degraded-operation misses vanish silently
/// fixable or suspicious failures return a cause string for later aggregation
fn tolerate<T>(result: StdResult<T, FetchError>) -> (Option<T>, Option<String>) {
    match result {
        Ok(value) => (Some(value), None),
        Err(FetchError::NotFound { .. } | FetchError::Transport(_)) => (None, None),
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

#[cfg(test)]
mod tests {
    use super::tolerate;
    use crate::fetch::http::FetchError;

    #[test]
    fn tolerate_swallows_absent_and_transport_silently() {
        assert_eq!(
            tolerate::<()>(Err(FetchError::NotFound { what: "x".into() })).1,
            None
        );
        assert_eq!(
            tolerate::<()>(Err(FetchError::Transport("down".into()))).1,
            None
        );
    }

    #[test]
    fn tolerate_surfaces_auth_and_github() {
        assert!(
            tolerate::<()>(Err(FetchError::Auth {
                what: "no token".into(),
            }))
            .1
            .is_some()
        );
        assert!(
            tolerate::<()>(Err(FetchError::Github("weird".into())))
                .1
                .is_some()
        );
    }
}
