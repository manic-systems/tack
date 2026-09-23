// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::BTreeMap,
    path::{
        Path,
        PathBuf,
    },
};

use gix::object::Kind as ObjectKind;
use misstep::{
    Result,
    ResultExt as _,
};

use crate::{
    error::user_bail,
    lock::{
        LockFile,
        LockedNode,
        SignedBy,
    },
    pins::{
        Input,
        PinsDoc,
    },
    project::Project,
    render::printable,
    report::{
        PinVerify,
        VerifyNote,
        VerifyOutcome,
        VerifyReport,
    },
    signers::{
        Anchor,
        Keyring,
        SignerName,
        key_file,
    },
    source::id::SourceId,
};

struct Base {
    lock:    LockFile,
    signers: BTreeMap<String, Vec<SignerName>>,
    keyring: Keyring,
}

enum TipReason {
    NewPin,
    SourceChanged,
    NoAnchor,
    NoBase,
    KeysChanged,
}

impl TipReason {
    const fn note(&self) -> Option<VerifyNote> {
        match *self {
            Self::NewPin => Some(VerifyNote::NewPin),
            Self::SourceChanged => Some(VerifyNote::SourceChanged),
            Self::NoAnchor => Some(VerifyNote::NoAnchor),
            Self::NoBase => Some(VerifyNote::NoBase),
            Self::KeysChanged => None,
        }
    }
}

pub fn verify(project: &Project, base: Option<&str>) -> Result<VerifyReport> {
    let doc = project.load_pins()?;
    let all = doc.inputs()?;
    let trusted = base.map(|rev| load_base(project, rev)).transpose()?;
    let gated = all
        .iter()
        .filter(|input| !input.signers.is_empty())
        .collect::<Vec<_>>();
    let dropped = trusted
        .iter()
        .flat_map(|old| old.signers.iter())
        .filter(|&(_, signers)| !signers.is_empty())
        .filter_map(|(name, _)| {
            let head = all.iter().find(|input| input.name == *name);
            head.is_none_or(|input| input.signers.is_empty())
                .then(|| (name, head.is_some()))
        })
        .collect::<Vec<_>>();
    if gated.is_empty() && dropped.is_empty() {
        return Ok(VerifyReport::default());
    }
    let lock = project.load_lock()?;
    let keyring = doc.keyring(&gated, project.dir())?;

    let mut pins = gated
        .into_iter()
        .map(|input| {
            let outcome = match check(input, &lock, trusted.as_ref(), &keyring) {
                Ok((signer, notes)) => VerifyOutcome::Signed { signer, notes },
                Err(err) => VerifyOutcome::Failed(format!("{err:#}")),
            };
            PinVerify {
                name: input.name.clone(),
                outcome,
            }
        })
        .collect::<Vec<_>>();
    pins.extend(dropped.into_iter().map(|(name, present)| {
        PinVerify {
            name:    name.clone(),
            outcome: if present {
                VerifyOutcome::Failed("signers removed since the base".to_owned())
            } else {
                VerifyOutcome::Removed
            },
        }
    }));
    Ok(VerifyReport { pins })
}

pub fn verify_cli(project: &Project, base: Option<&str>) -> Result<()> {
    let report = verify(project, base)?;
    if report.pins.is_empty() {
        println!("no pins have signers");
        return Ok(());
    }
    let width = report
        .pins
        .iter()
        .map(|pin| pin.name.chars().count())
        .max()
        .unwrap_or_default();
    for pin in &report.pins {
        let name = printable(&pin.name);
        match pin.outcome {
            VerifyOutcome::Signed {
                ref signer,
                ref notes,
            } => {
                let detail = notes.iter().map(describe).collect::<Vec<_>>().join(", ");
                let suffix = if detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({detail})")
                };
                println!(
                    "{name:<width$}  ok  signed by {}{suffix}",
                    printable(signer)
                );
            },
            VerifyOutcome::Removed => {
                println!("{name:<width$}  removed  had signers at the base, nothing to verify");
            },
            VerifyOutcome::Failed(ref reason) => {
                println!("{name:<width$}  FAILED  {}", printable(reason));
            },
        }
    }
    if let Some(message) = report.user_error() {
        user_bail!("{message}");
    }
    Ok(())
}

fn describe(note: &VerifyNote) -> String {
    let join = |items: &[String]| {
        items
            .iter()
            .map(|name| printable(name))
            .collect::<Vec<_>>()
            .join(" ")
    };
    match *note {
        VerifyNote::NewPin => "new pin".to_owned(),
        VerifyNote::SourceChanged => "source changed".to_owned(),
        VerifyNote::NoAnchor => "no verified anchor at the base".to_owned(),
        VerifyNote::NoBase => "no --base".to_owned(),
        VerifyNote::KeysChanged(ref changed) => format!("keys changed: {}", join(changed)),
        VerifyNote::TipOnly => "tip only".to_owned(),
        VerifyNote::SignersAdded(ref added) => format!("signers added: {}", join(added)),
        VerifyNote::RolledBack => "rolled back to an earlier verified commit".to_owned(),
    }
}

/// the current lock's `signedBy` is ignored, since a pull request can write
/// anything there, and only the merged base lock may vouch for a rev
fn check(
    input: &Input,
    lock: &LockFile,
    trusted: Option<&Base>,
    keyring: &Keyring,
) -> Result<(String, Vec<VerifyNote>)> {
    let Some(node) = lock.get(&input.name) else {
        user_bail!("not locked");
    };
    let mut changed = trusted
        .map(|old| changed_keys(input, old, keyring))
        .unwrap_or_default();
    let (vouched, tip) = match anchor_for(input, node, trusted) {
        Ok((rev, record)) if record.keys.is_some() => {
            if record.keys == current_keys(keyring, record) {
                (Some((rev, record)), None)
            } else {
                if !changed.contains(&record.signer) {
                    changed.push(record.signer.clone());
                }
                (None, Some(TipReason::KeysChanged))
            }
        },
        Ok(_) => (None, Some(TipReason::NoAnchor)),
        Err(reason) => (None, Some(reason)),
    };

    let (signer, rolled_back) = if let Some((rev, record)) = vouched
        && node.forge_rev() == Some(rev)
    {
        (record.signer.clone(), false)
    } else {
        let anchor = vouched.map(|(rev, record)| {
            Anchor {
                rev,
                since: record.since.as_deref(),
            }
        });
        let verdict = keyring.verify(&input.signers, anchor, node)?;
        (verdict.signer.to_string(), verdict.rolled_back)
    };

    let added = trusted
        .filter(|old| old.lock.get(&input.name).is_some())
        .and_then(|old| old.signers.get(&input.name))
        .map(|before| {
            input
                .signers
                .iter()
                .filter(|name| !before.contains(name))
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut notes = tip.iter().filter_map(TipReason::note).collect::<Vec<_>>();
    if !changed.is_empty() {
        notes.push(VerifyNote::KeysChanged(changed));
    }
    if tip.is_some() {
        notes.push(VerifyNote::TipOnly);
    }
    if !added.is_empty() {
        notes.push(VerifyNote::SignersAdded(added));
    }
    if rolled_back {
        notes.push(VerifyNote::RolledBack);
    }
    Ok((signer, notes))
}

fn anchor_for<'base>(
    input: &Input,
    node: &LockedNode,
    trusted: Option<&'base Base>,
) -> Result<(&'base str, &'base SignedBy), TipReason> {
    let old = trusted.ok_or(TipReason::NoBase)?;
    let before = old.lock.get(&input.name).ok_or(TipReason::NewPin)?;
    if SourceId::from_locked(before) != SourceId::from_locked(node) {
        return Err(TipReason::SourceChanged);
    }
    old.lock
        .signed_by(&input.name)
        .filter(|record| {
            input
                .signers
                .iter()
                .any(|name| name.as_str() == record.signer)
        })
        .zip(before.forge_rev())
        .map(|(record, rev)| (rev, record))
        .ok_or(TipReason::NoAnchor)
}

/// a signer counts as known at the base when the pin listed it or the base
/// declared its keys, and an unreadable base key reads as changed
fn changed_keys(input: &Input, old: &Base, keyring: &Keyring) -> Vec<String> {
    let before = old.signers.get(&input.name);
    input
        .signers
        .iter()
        .filter(|name| {
            let then = old.keyring.keys_digest(name);
            let listed = before.is_some_and(|signers| signers.contains(name));
            (listed || then.is_some()) && then != keyring.keys_digest(name)
        })
        .map(ToString::to_string)
        .collect()
}

fn current_keys(keyring: &Keyring, record: &SignedBy) -> Option<String> {
    keyring.keys_digest(&record.signer.parse::<SignerName>().ok()?)
}

/// absent at `rev` reads as an empty lock and no signers, so every pin is new
fn load_base(project: &Project, rev: &str) -> Result<Base> {
    let base = BaseTree::open(project, rev)?;
    let lock = base
        .read(Path::new("pins.lock.json"))?
        .map_or_else(|| Ok(LockFile::new()), |raw| LockFile::parse(&raw))
        .with_context(|| format!("parse pins.lock.json at {}", printable(rev)))?;
    let Some(raw) = base.read(Path::new("pins.toml"))? else {
        return Ok(Base {
            lock,
            signers: BTreeMap::new(),
            keyring: Keyring::default(),
        });
    };
    let doc = PinsDoc::parse(&raw)
        .map_err(misstep::Report::from)
        .with_context(|| format!("parse pins.toml at {}", printable(rev)))?;
    let signers = doc
        .inputs()?
        .into_iter()
        .map(|input| (input.name, input.signers))
        .collect();
    let keyring = Keyring::load_lenient(doc.signers()?, |value| {
        base.read(key_file(value).ok()?).ok().flatten()
    });
    Ok(Base {
        lock,
        signers,
        keyring,
    })
}

/// the base commit's tree, where a file missing from it is told apart from one
/// that failed to read
struct BaseTree {
    repo:   gix::Repository,
    tree:   gix::ObjectId,
    prefix: PathBuf,
}

impl BaseTree {
    fn open(project: &Project, rev: &str) -> Result<Self> {
        let repo = gix::discover(project.dir()).context("open the git repository")?;
        let Some(workdir) = repo.workdir() else {
            user_bail!("verify --base needs a git work tree");
        };
        let prefix = project
            .dir()
            .canonicalize()?
            .strip_prefix(workdir.canonicalize()?)
            .context(".tack is outside the git work tree")?
            .to_owned();
        let Ok(id) = repo.rev_parse_single(rev) else {
            user_bail!("'{}' is not a commit in this repository", printable(rev));
        };
        let tree = id
            .object()?
            .peel_to_commit()
            .with_context(|| format!("'{}' is not a commit", printable(rev)))?
            .tree_id()?
            .detach();
        Ok(Self { repo, tree, prefix })
    }

    /// [`None`] when `path`, relative to `.tack`, is not in the base tree
    fn read(&self, path: &Path) -> Result<Option<String>> {
        let tree = self.repo.find_tree(self.tree)?;
        let Some(entry) = tree.lookup_entry_by_path(self.prefix.join(path))? else {
            return Ok(None);
        };
        let shown = path.display();
        let object = entry.object()?;
        if object.kind != ObjectKind::Blob {
            user_bail!("{shown} at the base is not a file");
        }
        let text = String::from_utf8(object.data.clone())
            .with_context(|| format!("{shown} at the base is not utf-8"))?;
        Ok(Some(text))
    }
}
