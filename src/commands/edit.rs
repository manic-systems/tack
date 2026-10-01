// SPDX-License-Identifier: EUPL-1.2

use std::{
    fs,
    io::ErrorKind,
    path::Path,
};

use misstep::{
    Result,
    ResultExt as _,
};

use super::{
    Selection,
    select,
    update,
};
use crate::{
    cli::AddArgs,
    error::user_bail,
    fetch,
    patched::unroot,
    pins::{
        self,
        PinType,
    },
    project::Project,
    render,
    report::Dates,
    resolver,
    source,
    tag,
};

pub fn add(project: &Project, args: &AddArgs) -> Result<()> {
    let AddArgs {
        ref name,
        ref url,
        pin_type,
        unpack,
        ref dir,
        submodules,
        ref follows,
        ref template,
    } = *args;
    if unpack.is_some() && pin_type != PinType::Fixed {
        user_bail!("--unpack is only valid with --fixed");
    }
    if template.is_some() && pin_type == PinType::Fixed {
        user_bail!("--tag is not valid with --fixed");
    }
    let mut doc = project.load_pins()?;
    if doc.has_input(name) {
        user_bail!("input '{name}' already exists");
    }
    if doc
        .inputs()?
        .iter()
        .any(|input| input.group.as_deref() == Some(name))
    {
        user_bail!("'{name}' is already a group name");
    }
    let expanded = doc.shorturls().expand(url)?;
    if template.is_some() {
        tag::followable(name, &expanded)?;
        resolver::ensure(project, &[(name, resolver::TAG)])?;
    }
    doc.add_input(name, url, &pins::AddInputOpts {
        pin_type,
        unpack,
        dir: dir.as_deref(),
        submodules,
        follows,
        tag: template.as_ref(),
    });
    project.save_pins(&doc)?;

    let localized = source::localize_path_url_with_warning(&expanded, project.dir());
    if let Some(warning) = localized.warning {
        eprintln!("tack: {warning}");
    }
    let fetched = tag::follow(name, template.as_ref(), &localized.url).and_then(|followed| {
        update::fetch_input(pin_type, unpack, submodules, &followed.url)
            .map(|pin| (pin, followed.tag))
    });
    match fetched {
        Ok((fetched_pin, chosen)) => {
            let (node, identity) = fetched_pin.into_parts();
            let date = Dates::between(None, &node).new.map(render::date);
            let mut lk = project.load_lock()?;
            lk.insert(name.to_owned(), node);
            lk.set_declared(name, &expanded);
            let shown = chosen.as_deref().map_or_else(
                || render::added_identity(identity.as_str()),
                |tag| format!("NEW -> {tag}"),
            );
            lk.set_tag(name, chosen);
            project.save_lock(&lk)?;
            match date {
                Some(day) if !day.is_empty() => println!("added {name}  {shown} ({day})"),
                Some(_) | None => println!("added {name}  {shown}"),
            }
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
    unroot(&project, name)?;
    match fs::remove_dir_all(project.patches_dir().join(name)) {
        Err(err) if err.kind() != ErrorKind::NotFound => {
            return Err(err).with_context(|| format!("remove patches/{name}"));
        },
        _ => {},
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
        let Some(tpl) = template else {
            user_bail!("alias {name} needs a template, or --rm to remove it");
        };
        if !tpl.contains("{path}") {
            user_bail!("alias template must contain '{{path}}'");
        }
        doc.set_alias(name, tpl);
        project.save_pins(&doc)?;
        println!("alias {name} = {tpl}");
    }
    Ok(())
}

pub fn set_frozen(project: &Project, names: &[String], frozen: bool) -> Result<()> {
    if names.is_empty() {
        user_bail!("name at least one pin or group");
    }
    let mut doc = project.load_pins()?;
    let all = doc.inputs()?;
    let targets = select(&all, Selection::new(names, &[]))?;
    let (verb, unchanged) = if frozen {
        ("froze", "is already frozen")
    } else {
        ("unfroze", "isn't frozen")
    };
    let mut changed = false;
    for input in targets {
        if input.frozen == frozen {
            println!("{} {unchanged}", input.name);
        } else {
            doc.set_frozen(&input.name, frozen);
            println!("{verb} {}", input.name);
            changed = true;
        }
    }
    if changed {
        project.save_pins(&doc)?;
    }
    Ok(())
}
