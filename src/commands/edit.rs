// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::HashSet,
    path::Path,
};

use eyre::Result;

use super::{
    AddRequest,
    update,
};
use crate::{
    error::user_bail,
    fetch,
    pins::{
        self,
        PinType,
    },
    project::Project,
    render,
    source,
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
    let fetched = update::fetch_input(pin_type, unpack, submodules, &expanded);
    match fetched {
        Ok(fetched_pin) => {
            let (node, identity) = fetched_pin.into_parts();
            let mut lk = project.load_lock()?;
            lk.insert(name.to_owned(), node);
            project.save_lock(&lk)?;
            println!(
                "added {name}  {}",
                render::added_identity(identity.as_str())
            );
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

pub fn rm(project: &Project, name: Option<&str>, prune: bool) -> Result<()> {
    if name.is_none() && !prune {
        user_bail!("specify an input name or --prune");
    }

    let (removed_pin, removed_lock, stale) = rm_in_dir(project.dir(), name, prune)?;
    if let Some(name) = name {
        if removed_pin {
            println!("removed {name}");
        } else if removed_lock {
            println!("removed stale lock entry {name}");
        }
    }
    if !stale.is_empty() {
        let noun = if stale.len() == 1 { "entry" } else { "entries" };
        println!("pruned {} stale lock {noun}", stale.len());
    }
    Ok(())
}

fn rm_in_dir(dir: &Path, name: Option<&str>, prune: bool) -> Result<(bool, bool, Vec<String>)> {
    let project = Project::at(dir.to_owned());
    let mut doc = project.load_pins()?;
    let removed_pin = name.is_some_and(|name| doc.remove_input(name));

    let mut lk = project.load_lock()?;
    let removed_lock = name.is_some_and(|name| lk.remove(name));

    let stale = if prune {
        let inputs = doc
            .inputs()?
            .into_iter()
            .map(|input| input.name)
            .collect::<HashSet<_>>();
        let stale = lk
            .keys()
            .filter(|key| !inputs.contains(key.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for key in &stale {
            lk.remove(key);
        }
        stale
    } else {
        Vec::new()
    };

    if name.is_some() && !removed_pin && !removed_lock {
        user_bail!("no input '{}'", name.expect("checked above"));
    }

    if removed_pin {
        project.save_pins(&doc)?;
    }
    if removed_lock || !stale.is_empty() {
        project.save_lock(&lk)?;
    }
    Ok((removed_pin, removed_lock, stale))
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

    use super::rm;
    use crate::{
        LockFile,
        Project,
    };

    #[test]
    fn prune_removes_stale_recognized_lock_entries() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::at(dir.path().to_owned());
        fs::write(
            project.pins_path(),
            "[inputs.keep]\nurl = \"github:owner/repo\"\n",
        )
        .unwrap();
        fs::write(
            project.lock_path(),
            r#"{
  "keep": {"type": "github", "owner": "owner", "repo": "repo"},
  "stale": {"type": "github", "owner": "owner", "repo": "stale"},
  "future": {"type": "mercurial", "url": "https://example.test/repo"}
}
"#,
        )
        .unwrap();

        rm(&project, None, true).unwrap();

        let lock = LockFile::parse(&fs::read_to_string(project.lock_path()).unwrap()).unwrap();
        assert!(lock.get("keep").is_some());
        assert!(lock.get("stale").is_none());
        assert_eq!(lock.unknown_nodes().collect::<Vec<_>>(), vec!["future"]);
    }
}
