// SPDX-License-Identifier: EUPL-1.2

use std::fs;

use misstep::Result;

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
pub const TAG: &str = "tag";
pub const VERSION: &str = "version";
pub const SIGNED: &str = "signedBy";

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
            [
                (lock.patched(name).is_some(), PATCHED),
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
        .map(|&(pin, feature)| format!("'{pin}' uses {feature}"))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    if current.contains(MARKER) {
        project::write_atomic(&path, RESOLVER_NIX)?;
        eprintln!(
            "tack: refreshed resolver at {} to support {}",
            path.display(),
            missing.join(", ")
        );
        return Ok(());
    }
    user_bail!(
        "resolver at {} does not support lock features ({}). make it strip those attrs before \
         fetchTree, then list them on its `{FEATURES_PREFIX}` line",
        path.display(),
        missing.join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::{
        PATCHED,
        RESOLVER_NIX,
        SIGNED,
        TAG,
        VERSION,
        declared,
    };

    #[test]
    fn bundled_resolver_declares_every_lock_feature() {
        let features = declared(RESOLVER_NIX);
        assert!(
            [PATCHED, TAG, VERSION, SIGNED]
                .iter()
                .all(|feature| features.contains(feature))
        );
    }
}
