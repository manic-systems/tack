// SPDX-License-Identifier: EUPL-1.2

use std::path::Path;

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
            println!("added {name}  NEW -> {}", render::short(identity.as_str()));
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

fn rm_in_dir(dir: &Path, name: &str) -> Result<(bool, bool)> {
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
