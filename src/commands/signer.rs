// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::HashSet,
    fs,
    io::{
        ErrorKind,
        Write as _,
    },
    path::{
        Path,
        PathBuf,
    },
};

use misstep::{
    Result,
    ResultExt as _,
};
use tempfile::NamedTempFile;

use crate::{
    cli::SignerAction,
    error::user_bail,
    fetch::github::{
        self,
        GithubUser,
    },
    project::Project,
    render::printable,
    signers::{
        SignerKey,
        SignerName,
        confined,
        key_file,
    },
};

pub fn run(project: &Project, action: &SignerAction) -> Result<()> {
    match *action {
        SignerAction::Add {
            ref name,
            ref key,
            ref github,
        } => add(project, name, key.as_deref(), github.as_ref()),
        SignerAction::Rm { ref name } => rm(project, name),
        SignerAction::List => list(project),
    }
}

/// one value of the new signer's entry, where [`KeyValue::Written`] is a
/// file [`add`] writes under `.tack`
enum KeyValue {
    Inline(String),
    Existing(String),
    Written { path: String, contents: String },
}

impl KeyValue {
    fn written(name: &SignerName, key: &SignerKey, contents: String) -> Self {
        Self::Written {
            path: format!("keys/{name}.{}", key.extension()),
            contents,
        }
    }

    fn as_str(&self) -> &str {
        match *self {
            Self::Inline(ref value)
            | Self::Existing(ref value)
            | Self::Written {
                path: ref value, ..
            } => value,
        }
    }
}

/// every target is checked before any file lands, so a clash cannot leave some
/// keys written and others not
fn write_files(dir: &Path, values: &[KeyValue]) -> Result<Vec<PathBuf>> {
    let mut staged = Vec::new();
    for value in values {
        let KeyValue::Written {
            ref path,
            ref contents,
        } = *value
        else {
            continue;
        };
        let target = dir.join(path);
        if fs::symlink_metadata(&target).is_ok_and(|meta| meta.is_symlink()) {
            confined(dir, Path::new(path))?;
        }
        match fs::read_to_string(&target) {
            Ok(existing) if existing == *contents => continue,
            Ok(_) => user_bail!("{path} already exists, move it out of the way first"),
            Err(err) if err.kind() == ErrorKind::NotFound => {},
            Err(err) => return Err(err).with_context(|| format!("read {path}")),
        }
        let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
        fs::create_dir_all(dir.join(parent))?;
        confined(dir, parent)?;
        let mut temp = NamedTempFile::new_in(dir.join(parent))?;
        temp.write_all(contents.as_bytes())
            .with_context(|| format!("write {path}"))?;
        staged.push((temp, target));
    }

    let mut created = Vec::new();
    for (temp, target) in staged {
        if let Err(err) = temp.persist_noclobber(&target) {
            remove_files(&created);
            return Err(err.error).with_context(|| format!("write {}", target.display()));
        }
        created.push(target);
    }
    Ok(created)
}

fn remove_files(files: &[PathBuf]) {
    for file in files {
        match fs::remove_file(file) {
            Err(err) if err.kind() != ErrorKind::NotFound => {
                eprintln!("tack: could not clean up {}: {err}", file.display());
            },
            _ => {},
        }
    }
}

fn fingerprint_lines(dir: &Path, name: &SignerName, value: &str) -> Vec<String> {
    SignerKey::load(name, value, dir)
        .and_then(|key| key.fingerprints())
        .unwrap_or_else(|err| vec![printable(&format!("{}  {err:#}", printable(value)))])
}

fn print_fingerprints(dir: &Path, name: &SignerName, values: &[&str]) {
    for value in values {
        for line in fingerprint_lines(dir, name, value) {
            println!("  {line}");
        }
    }
}

fn add(
    project: &Project,
    name: &SignerName,
    key: Option<&str>,
    github: Option<&GithubUser>,
) -> Result<()> {
    let mut doc = project.load_pins()?;
    if doc.has_signer(name) {
        user_bail!("signer '{name}' already exists, remove it first with `tack signer rm {name}`");
    }
    let values = match (key, github) {
        (Some(raw), None) => vec![local_key(project.dir(), name, raw)?],
        (None, Some(user)) => github_keys(name, user)?,
        (Some(_), Some(_)) | (None, None) => {
            user_bail!("give signer '{name}' either a key or file, or --github <user>")
        },
    };

    let mut fingerprints = Vec::new();
    for value in &values {
        let parsed = match *value {
            KeyValue::Written { ref contents, .. } => contents.parse::<SignerKey>()?,
            KeyValue::Inline(ref raw) | KeyValue::Existing(ref raw) => {
                SignerKey::load(name, raw, project.dir())?
            },
        };
        fingerprints.extend(parsed.fingerprints()?);
    }

    let created = write_files(project.dir(), &values)?;
    let entry = values.iter().map(KeyValue::as_str).collect::<Vec<_>>();
    doc.add_signer(name, &entry);
    if let Err(err) = project.save_pins(&doc) {
        remove_files(&created);
        return Err(err);
    }
    println!("added signer {name}  {}", entry.join(", "));
    for line in fingerprints {
        println!("  {line}");
    }
    Ok(())
}

/// an SSH key line stays inline, while armored keys and files outside `.tack`
/// are kept under `keys/` so the project carries them
fn local_key(dir: &Path, name: &SignerName, raw: &str) -> Result<KeyValue> {
    if let Ok(key) = raw.parse::<SignerKey>() {
        return Ok(match key {
            SignerKey::Ssh(_) => KeyValue::Inline(raw.trim().to_owned()),
            SignerKey::Gpg(_) => KeyValue::written(name, &key, raw.to_owned()),
        });
    }
    let path = match Path::new(raw).canonicalize() {
        Ok(path) => path,
        Err(err) => user_bail!("'{raw}' is neither a public key nor a readable file: {err}"),
    };
    let contents = fs::read_to_string(&path).with_context(|| format!("read {raw}"))?;
    let key = contents
        .parse::<SignerKey>()
        .with_context(|| format!("signer '{name}': {raw}"))?;
    if let Ok(inside) = path.strip_prefix(dir.canonicalize()?) {
        return Ok(KeyValue::Existing(inside.to_string_lossy().into_owned()));
    }
    Ok(KeyValue::written(name, &key, contents))
}

fn github_keys(name: &SignerName, user: &GithubUser) -> Result<Vec<KeyValue>> {
    let ssh = github::ssh_signing_keys(user)?;
    let github::GpgKeys {
        armored: gpg,
        dropped,
    } = github::gpg_keys(user)?;
    if dropped > 0 {
        eprintln!(
            "tack: skipped {dropped} of {user}'s GPG keys that GitHub lists without key material"
        );
    }
    if ssh.is_empty() && gpg.is_empty() {
        user_bail!("{user} publishes no signing keys on GitHub");
    }

    let mut values = Vec::new();
    for keys in [ssh, gpg] {
        if keys.is_empty() {
            continue;
        }
        let mut contents = keys.join("\n");
        contents.push('\n');
        let key = contents
            .parse::<SignerKey>()
            .with_context(|| format!("{user}'s GitHub signing keys"))?;
        values.push(KeyValue::written(name, &key, contents));
    }
    Ok(values)
}

fn rm(project: &Project, name: &SignerName) -> Result<()> {
    let mut doc = project.load_pins()?;
    if !doc.has_signer(name) {
        user_bail!("no signer '{name}' in [signers]");
    }
    let users = doc
        .inputs()?
        .into_iter()
        .filter(|input| input.signers.contains(name))
        .map(|input| input.name)
        .collect::<Vec<_>>();
    if !users.is_empty() {
        user_bail!("signer '{name}' is still listed by {}", users.join(", "));
    }

    let shared = doc
        .signers()?
        .into_iter()
        .filter(|&(ref other, _)| other != name)
        .flat_map(|(_, values)| values)
        .map(PathBuf::from)
        .collect::<HashSet<_>>();
    let mut owned = Vec::new();
    for file in doc.signer_files(name) {
        let relative = key_file(&file)?.to_owned();
        if relative.starts_with("keys")
            && relative != Path::new("keys")
            && !shared.contains(&relative)
        {
            if let Some(parent) = relative.parent() {
                confined(project.dir(), parent)?;
            }
            owned.push(relative);
        }
    }

    doc.remove_signer(name);
    project.save_pins(&doc)?;
    for file in &owned {
        match fs::remove_file(project.dir().join(file)) {
            Err(err) if err.kind() != ErrorKind::NotFound => {
                return Err(err).with_context(|| format!("remove {}", file.display()));
            },
            _ => {},
        }
    }
    println!("removed signer {name}");
    Ok(())
}

fn list(project: &Project) -> Result<()> {
    let doc = project.load_pins()?;
    let inputs = doc.inputs()?;
    let signers = doc.signers()?;
    if signers.is_empty() {
        println!("no signers");
        return Ok(());
    }
    let width = signers
        .iter()
        .map(|&(ref name, _)| name.as_str().len())
        .max()
        .unwrap_or_default();
    for &(ref name, ref values) in &signers {
        let keys = values
            .iter()
            .map(|value| describe(value))
            .collect::<Vec<_>>()
            .join(", ");
        let users = inputs
            .iter()
            .filter(|input| input.signers.contains(name))
            .map(|input| input.name.as_str())
            .collect::<Vec<_>>();
        let pins = if users.is_empty() {
            "no pins".to_owned()
        } else {
            users.join(", ")
        };
        println!("{:width$}  {keys}  ({pins})", name.as_str());
        print_fingerprints(project.dir(), name, values);
    }
    Ok(())
}

/// an inline key by its type and comment, a key file by its path
fn describe(value: &str) -> String {
    match value.parse::<SignerKey>() {
        Ok(SignerKey::Ssh(lines)) => {
            lines
                .iter()
                .map(|line| {
                    let mut fields = line.split_whitespace();
                    let kind = fields.next().unwrap_or_default();
                    fields.nth(1).map_or_else(
                        || printable(kind),
                        |comment| printable(&format!("{kind} {comment}")),
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        },
        Ok(SignerKey::Gpg(_)) => "inline PGP key".to_owned(),
        Err(_) => printable(value),
    }
}
