// SPDX-License-Identifier: EUPL-1.2

use std::{
    fs,
    io::ErrorKind,
    path::{
        Path,
        PathBuf,
    },
};

use misstep::{
    Result,
    ResultExt as _,
};

use super::{
    Selection,
    select,
};
use crate::{
    cli::PatchAction,
    error::user_bail,
    patched::{
        Mode,
        PatchedPin,
        Settled,
        source::PatchSource,
    },
    pins::{
        Input,
        PinType,
        PinsDoc,
    },
    project::Project,
    resolver,
};

pub fn run(project: &Project, action: &PatchAction) -> Result<()> {
    match *action {
        PatchAction::Add {
            ref name,
            ref source,
        } => add(project, name, source),
        PatchAction::Update { ref names } => settle_selected(project, names, Mode::Refresh),
        PatchAction::Rm {
            ref name,
            ref source,
        } => rm(project, name, source),
    }
}

fn add(project: &Project, name: &str, raw_source: &str) -> Result<()> {
    let mut doc = project.load_pins()?;
    let inputs = doc.inputs()?;
    let input = patchable(&inputs, name)?;
    let mut lock = project.load_lock()?;
    let Some(node) = lock.get(name) else {
        user_bail!("input '{name}' is not locked yet, run `tack update {name}` first");
    };

    if raw_source.trim().is_empty() {
        user_bail!("no patch given for {name}");
    }
    resolver::ensure(project, &[(name, resolver::PATCHED)])?;
    let shorturls = doc.shorturls();
    let (source, copied) = if PatchSource::remote_url(raw_source, &shorturls)?.is_some() {
        (PatchSource::parse(raw_source, &shorturls)?, None)
    } else {
        let (relative, copied) = local_source(project, &inputs, name, raw_source)?;
        (PatchSource::Local(relative), copied)
    };
    if input.patches.contains(&source) {
        match source {
            PatchSource::Remote(_) => {
                user_bail!(
                    "{name} already has {source}, run `tack patch update {name}` to refresh it"
                )
            },
            PatchSource::Local(_) => {
                user_bail!(
                    "{name} already has {source}, edit it there and run `tack update {name}`"
                )
            },
        }
    }
    let sources = input
        .patches
        .iter()
        .chain([&source])
        .cloned()
        .collect::<Vec<_>>();
    let attempt = doc.add_patch(name, &source.to_string()).and_then(|()| {
        PatchedPin::new(project, name, node).settle(
            &sources,
            lock.patched(name),
            false,
            Mode::Update,
        )
    });
    let settled = match attempt {
        Ok(settled) => settled,
        Err(err) => {
            if let Some(copy) = copied {
                remove_if_present(&copy)?;
            }
            prune_if_empty(project, name);
            return Err(err);
        },
    };

    settled.record_into(&mut lock, name);
    project.save_pins(&doc)?;
    project.save_lock(&lock)?;
    println!("patched {name}  {source}");
    Ok(())
}

fn rm(project: &Project, name: &str, raw_source: &str) -> Result<()> {
    let mut doc = project.load_pins()?;
    let inputs = doc.inputs()?;
    let input = patchable(&inputs, name)?;
    let Some(source) = listed_source(project, &doc, input, raw_source)? else {
        user_bail!("{name} has no patch {raw_source}");
    };
    let remaining = input
        .patches
        .iter()
        .filter(|&existing| *existing != source)
        .cloned()
        .collect::<Vec<_>>();

    doc.remove_patch(name, &source.to_string())?;
    let mut lock = project.load_lock()?;
    if let Some(node) = lock.get(name) {
        let pin = PatchedPin::new(project, name, node);
        let settled = pin.settle(&remaining, lock.patched(name), false, Mode::Update)?;
        settled.record_into(&mut lock, name);
    }
    project.save_pins(&doc)?;
    project.save_lock(&lock)?;
    let kept = match source {
        PatchSource::Remote(ref remote) => {
            let file = remote.vendored_file(name);
            remove_if_present(&project.dir().join(&file))?;
            file
        },
        PatchSource::Local(ref file) => file.clone(),
    };
    remove_if_present(&project.dir().join(format!("{kept}.rej")))?;
    prune_if_empty(project, name);
    println!("removed {source} from {name}");
    Ok(())
}

/// the source as pins.toml lists it, also found from a path to a local patch
/// written relative to the current directory or to `.tack`
fn listed_source(
    project: &Project,
    doc: &PinsDoc,
    input: &Input,
    raw: &str,
) -> Result<Option<PatchSource>> {
    if let Ok(source) = PatchSource::parse(raw, &doc.shorturls())
        && input.patches.contains(&source)
    {
        return Ok(Some(source));
    }
    let root = project.dir().canonicalize()?;
    let found = [PathBuf::from(raw), project.dir().join(raw)]
        .into_iter()
        .filter_map(|path| path.canonicalize().ok())
        .find_map(|path| {
            // a file from outside `.tack` is listed as the copy `add` made
            let relative = path.strip_prefix(&root).map_or_else(
                |_| {
                    let file_name = path.file_name()?.to_string_lossy();
                    Some(format!("patches/{}/{file_name}", input.name))
                },
                |inside| Some(inside.to_string_lossy().into_owned()),
            )?;
            let local = PatchSource::Local(relative);
            input.patches.contains(&local).then_some(local)
        });
    Ok(found)
}

pub fn materialize(project: &Project, names: &[String]) -> Result<()> {
    settle_selected(project, names, Mode::Restore)
}

fn settle_selected(project: &Project, names: &[String], mode: Mode) -> Result<()> {
    let doc = project.load_pins()?;
    let inputs = doc.inputs()?;
    let mut lock = project.load_lock()?;
    let mut changed = false;
    let mut failed = 0_usize;
    for input in select(&inputs, Selection::new(names, &[]))? {
        let Some(node) = lock.get(&input.name) else {
            continue;
        };
        let pin = PatchedPin::new(project, &input.name, node);
        match pin.settle(&input.patches, lock.patched(&input.name), false, mode) {
            Ok(settled) => {
                let status = match (mode, &settled) {
                    (_, &Settled::Unpatched) => None,
                    (Mode::Refresh, &(Settled::Current | Settled::Restored)) => Some("unchanged"),
                    (Mode::Refresh, &Settled::Rebuilt(_)) => Some("refreshed"),
                    (_, &Settled::Current) => Some("present"),
                    (_, &Settled::Restored) => Some("restored"),
                    (_, &Settled::Rebuilt(_)) => Some("rebuilt"),
                    (_, &Settled::Rehashed(_)) => {
                        Some("rebuilt, it no longer matched the lock's hash")
                    },
                };
                if let Some(label) = status {
                    println!("{}  {label}", input.name);
                }
                changed |= settled.record_into(&mut lock, &input.name);
            },
            Err(err) => {
                eprintln!("tack: {}: {err:#}", input.name);
                failed += 1;
            },
        }
    }
    if changed {
        project.save_lock(&lock)?;
    }
    if failed > 0 {
        user_bail!("{failed} pin(s) failed to settle their patches");
    }
    Ok(())
}

fn patchable<'a>(inputs: &'a [Input], name: &str) -> Result<&'a Input> {
    let Some(input) = inputs.iter().find(|input| input.name == name) else {
        user_bail!("no input '{name}'");
    };
    if input.pin_type == PinType::Fixed {
        user_bail!("input '{name}' is a fixed pin, which cannot take patches");
    }
    Ok(input)
}

/// a patch inside the .tack dir is referenced in place, anything else is
/// copied under `patches/<pin>/` so pure eval can read it, and `raw` may be
/// relative to the current directory or to `.tack`
fn local_source(
    project: &Project,
    inputs: &[Input],
    pin: &str,
    raw: &str,
) -> Result<(String, Option<PathBuf>)> {
    let found = [PathBuf::from(raw), project.dir().join(raw)]
        .into_iter()
        .find_map(|path| path.canonicalize().ok());
    let Some(canonical) = found else {
        user_bail!("read {raw}: no such file");
    };
    let root = project.dir().canonicalize()?;
    if let Ok(inside) = canonical.strip_prefix(&root) {
        return Ok((inside.to_string_lossy().into_owned(), None));
    }
    let Some(file_name) = canonical.file_name() else {
        user_bail!("{raw}: not a file");
    };
    let relative = format!("patches/{pin}/{}", file_name.to_string_lossy());
    let target = project.dir().join(&relative);
    let referenced = inputs.iter().any(|input| {
        input
            .patches
            .contains(&PatchSource::Local(relative.clone()))
    });
    if referenced && target.exists() {
        if fs::read(&target)? == fs::read(&canonical)? {
            return Ok((relative, None));
        }
        user_bail!("{relative} already exists, rename the patch first");
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::copy(&canonical, &target).with_context(|| format!("copy {raw}"))?;
    Ok((relative, Some(target)))
}

/// a pin's patch dir, and then `patches/` itself, go once nothing is left
fn prune_if_empty(project: &Project, pin: &str) {
    let dir = project.patches_dir();
    let _ = fs::remove_dir(dir.join(pin));
    let _ = fs::remove_dir(dir);
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != ErrorKind::NotFound => {
            Err(err).with_context(|| format!("remove {}", path.display()))
        },
        _ => Ok(()),
    }
}
