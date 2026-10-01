// SPDX-License-Identifier: EUPL-1.2

mod view;

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
        HashSet,
    },
    fs,
    io::ErrorKind,
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
    dispatcher,
    fetch,
    lock::{
        FlakeInputRef,
        FlakeLock,
        LockFile,
    },
    pins::{
        self,
        PinType,
        Side,
    },
    project::Project,
    report::{
        LockedSource,
        PinLock,
        PinTree,
        TreeInput,
        TreeReport,
        TreeTarget,
    },
};

const TREE_IN_FLIGHT: usize = 16;
/// real locks nest a handful of levels, so this only stops hostile chains
/// before the recursive walk and view exhaust a worker's stack
const MAX_DEPTH: usize = 64;

pub fn tree(project: &Project, selection: Selection<'_>) -> Result<TreeReport> {
    let doc = project.load_pins()?;
    let all = doc.inputs()?;
    let all_follow = doc.all_follows()?;
    let omit_inputs = doc.omit_inputs()?;
    let lock = project.load_lock()?;
    let selected = select(&all, selection)?;
    let trees = dispatcher::ordered(selected, TREE_IN_FLIGHT, |_, input| {
        pin_tree(input, &all_follow, &omit_inputs, &lock)
    });

    let mut warnings = Vec::new();
    let pins = trees
        .into_iter()
        .map(|(pin, warning)| {
            warnings.extend(warning);
            pin
        })
        .collect::<Vec<_>>();
    Ok(TreeReport { pins, warnings })
}

pub fn tree_cli(project: &Project, selection: Selection<'_>) -> Result<()> {
    view::print(&tree(project, selection)?);
    Ok(())
}

fn pin_tree(
    input: &pins::Input,
    all_follow: &BTreeMap<String, String>,
    omit_inputs: &BTreeSet<String>,
    lock: &LockFile,
) -> (PinTree, Option<String>) {
    let locked = lock.get(&input.name);
    let mut tree = PinTree {
        name:   input.name.clone(),
        group:  input.group.clone(),
        lock:   match (locked, lock.unknown_type(&input.name)) {
            (Some(found), _) => PinLock::Locked(LockedSource::from(found)),
            (None, Some(kind)) => PinLock::Unknown(kind.to_owned()),
            (None, None) => PinLock::Missing,
        },
        inputs: Vec::new(),
    };
    let Some(node) = locked else {
        return (tree, None);
    };
    if input.pin_type != PinType::Flake {
        return (tree, None);
    }
    let path = input.dir.as_deref().map_or_else(
        || "flake.lock".to_owned(),
        |subdir| format!("{subdir}/flake.lock"),
    );
    let parsed = patched_file(lock, &input.name, &path)
        .and_then(|found| {
            found.map_or_else(|| fetch::locked_file(node, &path), |raw| Ok(Some(raw)))
        })
        .and_then(|raw| {
            raw.map(|body| FlakeLock::parse(&body).with_context(|| format!("parse {path}")))
                .transpose()
        });
    let flake_lock = match parsed {
        Ok(Some(flake_lock)) => flake_lock,
        Ok(None) => return (tree, None),
        Err(err) => {
            let warning = format!("{}: could not read its flake.lock: {err:#}", input.name);
            return (tree, Some(warning));
        },
    };

    let wiring = Wiring::for_pin(input, all_follow, omit_inputs);
    let mut walk = Walk::new(&flake_lock, wiring);
    tree.inputs = walk.inputs(flake_lock.root(), 0);
    let warning = walk.truncated.then(|| {
        format!(
            "{}: flake.lock nests inputs deeper than {MAX_DEPTH} levels, deeper ones are not shown",
            input.name
        )
    });
    (tree, warning)
}

/// [`None`] when the pin is unpatched or its patched tree is not in this store
fn patched_file(lock: &LockFile, name: &str, path: &str) -> Result<Option<String>> {
    let Some(tree) = lock.patched(name).filter(|tree| tree.path.exists()) else {
        return Ok(None);
    };
    match fs::read_to_string(tree.path.as_path().join(path)) {
        Ok(body) => Ok(Some(body)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read patched {path}")),
    }
}

/// the resolver applies a pin's own follows on its first level only, and
/// `[all_follow]` and omits at every depth
struct Wiring<'a> {
    first:         BTreeMap<&'a str, &'a str>,
    deeper:        BTreeMap<&'a str, &'a str>,
    omitted:       BTreeSet<String>,
    kept:          &'a BTreeSet<String>,
    /// first-level inputs other pins follow into, which an omit can't drop
    self_followed: BTreeSet<&'a str>,
}

impl<'a> Wiring<'a> {
    fn for_pin(
        input: &'a pins::Input,
        all_follow: &'a BTreeMap<String, String>,
        omit_inputs: &BTreeSet<String>,
    ) -> Self {
        let into_self = format!("{}/", input.name);
        let mut first = BTreeMap::new();
        let mut deeper = BTreeMap::new();
        let mut self_followed = BTreeSet::new();
        for (alias, target) in all_follow {
            let Some(name) = pins::rule_name(alias, Side::Flake) else {
                continue;
            };
            let into = target.starts_with(&into_self);
            if into {
                self_followed.insert(name);
            }
            if pins::rules_match(&input.excludes, Side::Flake, name) {
                continue;
            }
            deeper.insert(name, target.as_str());
            if !into {
                first.insert(name, target.as_str());
            }
        }
        for (alias, target) in &input.follows {
            if let Some(name) = pins::rule_name(alias, Side::Flake) {
                first.insert(name, target.as_str());
            }
        }
        Self {
            first,
            deeper,
            omitted: omit_inputs.union(&input.omit_inputs).cloned().collect(),
            kept: &input.keep_inputs,
            self_followed,
        }
    }

    fn omits(&self, input: &str, depth: usize) -> bool {
        !(depth == 0 && self.self_followed.contains(input))
            && pins::rules_match(&self.omitted, Side::Flake, input)
            && !pins::rules_match(self.kept, Side::Flake, input)
    }

    fn target(&self, input: &str, depth: usize) -> Option<&'a str> {
        let level = if depth == 0 {
            &self.first
        } else {
            &self.deeper
        };
        level.get(input).copied()
    }
}

struct Walk<'a> {
    flake_lock: &'a FlakeLock,
    wiring:     Wiring<'a>,
    /// below the first level every node sees the same follows, so listing a
    /// node's inputs once is exact, and a lock sharing nodes between many
    /// parents stays linear instead of exponential
    expanded:   HashSet<&'a str>,
    truncated:  bool,
}

impl<'a> Walk<'a> {
    fn new(flake_lock: &'a FlakeLock, wiring: Wiring<'a>) -> Self {
        Self {
            flake_lock,
            wiring,
            expanded: HashSet::from([flake_lock.root()]),
            truncated: false,
        }
    }

    fn inputs(&mut self, node: &str, depth: usize) -> Vec<TreeInput> {
        let flake_lock = self.flake_lock;
        flake_lock
            .inputs(node)
            .filter_map(|(name, input)| {
                let target = if let Some(pin) = self.wiring.target(name, depth) {
                    TreeTarget::FollowsPin(pin.to_owned())
                } else if self.wiring.omits(name, depth) {
                    TreeTarget::Omitted
                } else {
                    match *input {
                        FlakeInputRef::Follows(ref path) => TreeTarget::FollowsInput(path.clone()),
                        FlakeInputRef::Node(ref child) => self.node(child, depth + 1)?,
                    }
                };
                Some(TreeInput {
                    name: name.to_owned(),
                    target,
                })
            })
            .collect()
    }

    fn node(&mut self, node: &'a str, depth: usize) -> Option<TreeTarget> {
        let Some(locked) = self.flake_lock.locked(node) else {
            let kind = self.flake_lock.unknown_type(node)?;
            return Some(TreeTarget::Unknown(kind.to_owned()));
        };
        let source = LockedSource::from(locked);
        if !self.expanded.insert(node) {
            return Some(TreeTarget::Repeated(source));
        }
        let inputs = if depth < MAX_DEPTH {
            self.inputs(node, depth)
        } else {
            self.truncated = true;
            Vec::new()
        };
        Some(TreeTarget::Locked { source, inputs })
    }
}

#[cfg(test)]
#[path = "tree_tests.rs"]
mod tests;
