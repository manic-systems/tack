// SPDX-License-Identifier: EUPL-1.2

use std::{
    fs,
    io,
    path::Path,
};

use misstep::{
    Result,
    ResultExt as _,
};

use crate::{
    error::user_bail,
    lock::LockFile,
    project::{
        self,
        Project,
    },
};

pub const RESOLVER_NIX: &str = include_str!("../.tack/default.nix");
pub const MARKER: &str = "# tack-managed resolver.";
const FEATURES_PREFIX: &str = "# tack-resolver:";

pub const PATCHED: &str = "patched";
pub const PATCHED_DIFF: &str = "patchedDiff";
pub const TAG: &str = "tag";
pub const VERSION: &str = "version";
pub const SIGNED: &str = "signedBy";

/// lock features an older resolver can safely ignore
const OPTIONAL: [&str; 1] = [PATCHED_DIFF];

fn declared(resolver: &str) -> Vec<&str> {
    resolver
        .lines()
        .filter_map(|line| line.strip_prefix(FEATURES_PREFIX))
        .flat_map(str::split_whitespace)
        .collect()
}

pub fn ensure_lock(project: &Project, lock: &LockFile) -> Result<()> {
    let needs = lock
        .keys()
        .flat_map(|name| {
            let patched = lock.patched(name);
            [
                (patched.is_some(), PATCHED),
                (
                    patched.is_some_and(|tree| tree.diff.is_some()),
                    PATCHED_DIFF,
                ),
                (lock.tag(name).is_some(), TAG),
                (lock.version(name).is_some(), VERSION),
                (lock.signed_by(name).is_some(), SIGNED),
            ]
            .into_iter()
            .filter(|&(used, _)| used)
            .map(move |(_, feature)| (name.as_str(), feature))
        })
        .collect::<Vec<_>>();
    ensure(project, &needs)
}

pub fn ensure(project: &Project, needs: &[(&str, &'static str)]) -> Result<()> {
    if needs.is_empty() {
        return Ok(());
    }
    let path = project.resolver_path();
    let Ok(current) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let known = declared(&current);
    let missing = needs
        .iter()
        .filter(|&&(_, feature)| !known.contains(&feature))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    let describe = |features: &[&(&str, &str)]| {
        features
            .iter()
            .map(|&&(pin, feature)| format!("'{pin}' uses {feature}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if current.contains(MARKER) {
        project::write_atomic(&path, RESOLVER_NIX)?;
        ignore_out_links(project.dir())?;
        eprintln!(
            "tack: refreshed resolver at {} to support {}",
            path.display(),
            describe(&missing)
        );
        return Ok(());
    }
    let required = missing
        .into_iter()
        .filter(|&&(_, feature)| !OPTIONAL.contains(&feature))
        .collect::<Vec<_>>();
    if required.is_empty() {
        return Ok(());
    }
    user_bail!(
        "resolver at {} does not support lock features ({}). make it strip those attrs before \
         fetchTree, then list them on its `{FEATURES_PREFIX}` line",
        path.display(),
        describe(&required)
    );
}

const OUT_LINKS: [&str; 2] = ["/patched", "/patched-*"];

/// keeps the links `nix-build .tack -A _patched -o .tack/patched` leaves out
/// of git, leaving any other ignore rules alone
pub fn ignore_out_links(dir: &Path) -> Result<()> {
    let path = dir.join(".gitignore");
    let current = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let missing = OUT_LINKS
        .into_iter()
        .filter(|rule| !current.lines().any(|line| line.trim() == *rule))
        .flat_map(|rule| [rule, "\n"])
        .collect::<String>();
    if missing.is_empty() {
        return Ok(());
    }
    let separator = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    project::write_atomic(&path, format!("{current}{separator}{missing}"))
}

#[cfg(test)]
#[path = "resolver_tests.rs"]
mod tests;
