// SPDX-License-Identifier: EUPL-1.2

use super::{
    Path,
    PinType,
    Project,
    Result,
    Source,
    Unpack,
    bail,
    fetch,
    pins,
    render,
};

pub fn add(
    name: &str,
    url: &str,
    pin_type: PinType,
    unpack: Option<Unpack>,
    dir_field: Option<&str>,
    submodules: bool,
    follows: &[(String, String)],
) -> Result<()> {
    if unpack.is_some() && pin_type != PinType::Fixed {
        bail!("--unpack is only valid with --fixed");
    }
    let project = Project::discover();
    let mut doc = project.load_pins()?;
    if doc.has_input(name) {
        bail!("input '{name}' already exists");
    }
    doc.add_input(name, url, &pins::AddInputOpts {
        pin_type,
        unpack,
        dir: dir_field,
        submodules,
        follows,
    });
    project.save_pins(&doc)?;

    let shorturls = doc.shorturls();
    let expanded = shorturls.expand(url);
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
    Ok(())
}

pub fn rm(name: &str) -> Result<()> {
    let project = Project::discover();
    let (removed_pin, removed_lock) = rm_in_dir(project.dir(), name)?;
    if removed_pin {
        println!("removed {name}");
    } else if removed_lock {
        println!("removed stale lock entry {name}");
    }
    Ok(())
}

pub(in crate::commands) fn rm_in_dir(dir: &Path, name: &str) -> Result<(bool, bool)> {
    let project = Project::at(dir.to_owned());
    let mut doc = project.load_pins()?;
    let removed_pin = doc.remove_input(name);

    let mut lk = project.load_lock()?;
    let removed_lock = lk.remove(name);

    if !removed_pin && !removed_lock {
        bail!("no input '{name}'");
    }

    if removed_pin {
        project.save_pins(&doc)?;
    }
    if removed_lock {
        project.save_lock(&lk)?;
    }
    Ok((removed_pin, removed_lock))
}

pub fn alias(name: &str, template: Option<&str>, remove: bool) -> Result<()> {
    let project = Project::discover();
    let mut doc = project.load_pins()?;
    if remove {
        if !doc.remove_alias(name) {
            bail!("no alias '{name}'");
        }
        project.save_pins(&doc)?;
        println!("removed alias {name}");
    } else {
        let tpl = template.expect("template required");
        if !tpl.contains("{path}") {
            bail!("alias template must contain '{{path}}'");
        }
        doc.set_alias(name, tpl);
        project.save_pins(&doc)?;
        println!("alias {name} = {tpl}");
    }
    Ok(())
}
