// SPDX-License-Identifier: EUPL-1.2

use std::path::Path;

use eyre::Result;

use super::AddRequest;
use crate::{
    error::user_bail,
    fetch,
    pins::{
        self,
        PinType,
    },
    project::Project,
    render,
    source::{
        self,
        Source,
    },
};

pub fn add(project: &Project, request: AddRequest<'_>) -> Result<()> {
    let AddRequest {
        name,
        url,
        pin_type,
        unpack,
        dir,
        submodules,
        follows,
    } = request;
    if unpack.is_some() && pin_type != PinType::Fixed {
        user_bail!("--unpack is only valid with --fixed");
    }
    let mut doc = project.load_pins()?;
    if doc.has_input(name) {
        user_bail!("input '{name}' already exists");
    }
    doc.add_input(name, url, &pins::AddInputOpts {
        pin_type,
        unpack,
        dir,
        submodules,
        follows,
    });
    project.save_pins(&doc)?;

    let shorturls = doc.shorturls();
    let localized = source::localize_path_url_with_warning(&shorturls.expand(url), project.dir());
    if let Some(warning) = localized.warning {
        eprintln!("tack: {warning}");
    }
    let expanded = localized.url;
    let fetched = match pin_type {
        PinType::Fixed => fetch::fetch_fixed_pin(&expanded, unpack),
        PinType::Flake | PinType::Fetch => {
            expanded
                .parse::<Source>()
                .and_then(|source| fetch::fetch_pin(&source, submodules))
        },
    };
    match fetched {
        Ok((node, rev)) => {
            let mut lk = project.load_lock()?;
            lk.insert(name.to_owned(), node);
            project.save_lock(&lk)?;
            println!("added {name}  NEW -> {}", render::short(&rev));
        },
        Err(err) => {
            println!("added {name} to pins.toml, but locking failed: {err:#}");
            println!("  fix the url and run `tack update {name}`");
        },
    }
    for warning in fetch::drain_fetch_warnings() {
        eprintln!("tack: {warning}");
    }
    Ok(())
}

pub fn rm(project: &Project, name: &str) -> Result<()> {
    let (removed_pin, removed_lock) = rm_in_dir(project.dir(), name)?;
    if removed_pin {
        println!("removed {name}");
    } else if removed_lock {
        println!("removed stale lock entry {name}");
    }
    Ok(())
}

pub(super) fn rm_in_dir(dir: &Path, name: &str) -> Result<(bool, bool)> {
    let project = Project::at(dir.to_owned());
    let mut doc = project.load_pins()?;
    let removed_pin = doc.remove_input(name);

    let mut lk = project.load_lock()?;
    let removed_lock = lk.remove(name);

    if !removed_pin && !removed_lock {
        user_bail!("no input '{name}'");
    }

    if removed_pin {
        project.save_pins(&doc)?;
    }
    if removed_lock {
        project.save_lock(&lk)?;
    }
    Ok((removed_pin, removed_lock))
}

pub fn alias(project: &Project, name: &str, template: Option<&str>, remove: bool) -> Result<()> {
    let mut doc = project.load_pins()?;
    if remove {
        if !doc.remove_alias(name) {
            user_bail!("no alias '{name}'");
        }
        project.save_pins(&doc)?;
        println!("removed alias {name}");
    } else {
        let tpl = template.expect("template required");
        if !tpl.contains("{path}") {
            user_bail!("alias template must contain '{{path}}'");
        }
        doc.set_alias(name, tpl);
        project.save_pins(&doc)?;
        println!("alias {name} = {tpl}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::rm_in_dir;

    #[test]
    fn rm_removes_orphaned_lock_entry() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pins.toml"), "[inputs]\n").unwrap();
        fs::write(
            dir.path().join("pins.lock.json"),
            r#"{"gone":{"type":"github","owner":"o","repo":"r","rev":"bad","narHash":"sha256-x"}}"#,
        )
        .unwrap();

        assert_eq!(rm_in_dir(dir.path(), "gone").unwrap(), (false, true));
        assert_eq!(
            fs::read_to_string(dir.path().join("pins.toml")).unwrap(),
            "[inputs]\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("pins.lock.json")).unwrap(),
            "{}\n"
        );
    }

    #[test]
    fn rm_errors_when_pin_and_lock_are_missing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pins.toml"), "[inputs]\n").unwrap();
        fs::write(dir.path().join("pins.lock.json"), "{}\n").unwrap();

        let err = rm_in_dir(dir.path(), "missing").unwrap_err().to_string();
        assert_eq!(err, "no input 'missing'");
    }
}
