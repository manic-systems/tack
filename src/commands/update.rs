// SPDX-License-Identifier: EUPL-1.2

use rayon::prelude::{
    IndexedParallelIterator as _,
    IntoParallelRefIterator as _,
    ParallelIterator as _,
};

use super::{
    AtomicUsize,
    BTreeMap,
    BranchComparison,
    CompareStatus,
    Display,
    HashMap,
    HashSet,
    Mutex,
    Ordering,
    PinStatus,
    PinType,
    Project,
    Result,
    Source,
    Value,
    bail,
    dedup::{
        strip_disambiguator,
        try_raw_file,
    },
    fetch,
    iter,
    lock,
    pins,
    render,
    select,
    shorturl,
};

struct UpdateFetch {
    node:       Value,
    rev:        String,
    comparison: BranchComparison,
}

pub fn update(names: &[String], accept: bool) -> Result<()> {
    let project = Project::discover();
    let doc = project.load_pins()?;
    let shorturls = pins::shorturls(&doc);
    let all = pins::inputs(&doc)?;
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(());
    }
    let mut lk = project.load_lock()?;

    let display = Display::new(selected.iter().map(|i| i.name.clone()).collect());
    let drift = AtomicUsize::new(0);

    let results = selected
        .par_iter()
        .enumerate()
        .map(|(i, inp)| {
            display.set(i, PinStatus::Fetching);
            let expanded = shorturl::expand(&inp.url, &shorturls);
            let old = lk.get(&inp.name);
            let old_rev = old.and_then(|n| lock::Node::from(n).rev());
            let fetched = fetch_for_update(inp, &expanded, old_rev);
            match fetched {
                // for fixed pins sha256 is the identity; any mismatch is drift
                Ok(UpdateFetch { node, rev, .. })
                    if inp.pin_type == PinType::Fixed
                        && old_rev.is_some()
                        && old_rev != Some(rev.as_str()) =>
                {
                    display.set(i, PinStatus::FixedDrift {
                        old:      old_rev.map(render::short).unwrap_or_default(),
                        new:      render::short(&rev),
                        accepted: accept,
                    });
                    if accept {
                        Some(node)
                    } else {
                        drift.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                },
                Ok(UpdateFetch { node, rev, .. }) if old_rev == Some(rev.as_str()) => {
                    // same rev, if hash moved, upstream changed under a stable rev
                    let drifted = hash_drifted(old, &node);
                    if drifted {
                        display.set(i, PinStatus::Drift {
                            rev:      render::short(&rev),
                            accepted: accept,
                        });
                        if accept {
                            Some(node)
                        } else {
                            drift.fetch_add(1, Ordering::Relaxed);
                            None
                        }
                    } else {
                        display.set(i, PinStatus::NoChange);
                        None
                    }
                },
                Ok(UpdateFetch {
                    node,
                    rev,
                    comparison,
                }) => {
                    display.set(i, PinStatus::Updated {
                        old: old_rev.map_or_else(|| "NEW".into(), render::short),
                        new: render::short(&rev),
                        comparison,
                    });
                    Some(node)
                },
                Err(err) => {
                    display.set(i, PinStatus::Failed(format!("{err:#}")));
                    None
                },
            }
        })
        .collect::<Vec<Option<Value>>>();

    let mut changed = false;
    for (inp, result) in selected.iter().zip(results) {
        if let Some(node) = result {
            lk.insert(inp.name.clone(), node);
            changed = true;
        }
    }
    let all_follow = pins::all_follows(&doc);
    if write_auto_dedup(&all, &all_follow, &mut lk) {
        changed = true;
    }
    if changed {
        project.save_lock(&lk)?;
    }
    display.finish();

    if drift.into_inner() > 0 {
        bail!(
            "upstream content differs from lock (lock kept; investigate, then re-run with \
             --accept to relock)"
        );
    }
    Ok(())
}

fn hash_drifted(old: Option<&Value>, node: &Value) -> bool {
    matches!(
        (
            old.and_then(|n| lock::Node::from(n).hash()),
            lock::Node::from(node).hash()
        ),
        (Some(prev), Some(curr)) if prev != curr
    )
}

fn fetch_for_update(
    inp: &pins::Input,
    expanded: &str,
    old_rev: Option<&str>,
) -> Result<UpdateFetch> {
    match inp.pin_type {
        PinType::Fixed => {
            fetch::fetch_fixed_pin(expanded, inp.unpack).map(|(node, rev)| {
                UpdateFetch {
                    node,
                    rev,
                    comparison: BranchComparison::none(),
                }
            })
        },
        PinType::Flake | PinType::Fetch => {
            let source = expanded.parse::<Source>()?;
            fetch::fetch_pin_compared(&source, inp.submodules, old_rev).map(|fetched| {
                UpdateFetch {
                    node:       fetched.node,
                    rev:        fetched.rev,
                    comparison: fetched.comparison,
                }
            })
        },
    }
}

/// for every `[all_follow]` entry whose target isn't a declared `[inputs]` pin,
/// walk all top-level flake.locks once, collect every transitive observation
/// of the aliased name, and write the freshest by branch comparison when
/// possible, falling back to `lastModified`. also prunes stale auto-dedup
/// entries that no longer have a route.
fn write_auto_dedup(
    inputs: &[pins::Input],
    all_follow: &BTreeMap<String, String>,
    lock: &mut lock::Lock,
) -> bool {
    let input_names = inputs
        .iter()
        .map(|i| i.name.as_str())
        .collect::<HashSet<&str>>();

    // synthesis observes flake.lock nodes, so project aliases onto the flake
    // side: `flake:` is rekeyed bare and `tack:` is dropped
    let aliases = all_follow
        .iter()
        .filter(|&(_, target)| !input_names.contains(target.as_str()))
        .filter_map(|(alias, target)| Some((pins::flake_side(alias)?.to_owned(), target.clone())))
        .collect::<BTreeMap<String, String>>();

    let mut valid = inputs
        .iter()
        .map(|i| i.name.clone())
        .collect::<HashSet<String>>();
    for target in aliases.values() {
        valid.insert(target.clone());
    }

    let stale = lock
        .keys()
        .filter(|key| !valid.contains(key.as_str()))
        .cloned()
        .collect::<Vec<String>>();
    let mut changed = false;
    for key in stale {
        lock.remove(&key);
        changed = true;
    }

    if aliases.is_empty() {
        return changed;
    }

    let batches = {
        let lock_ro: &lock::Lock = lock;
        inputs
            .par_iter()
            .filter(|inp| inp.pin_type == PinType::Flake)
            .filter_map(|inp| {
                let node = lock_ro.get(&inp.name)?;
                let raw = try_raw_file(node, "flake.lock")?;
                let parsed = serde_json::from_str::<Value>(&raw).ok()?;
                let root_key = parsed
                    .get("root")
                    .and_then(Value::as_str)
                    .unwrap_or("root")
                    .to_owned();
                let nodes = parsed.get("nodes")?.as_object()?.clone();
                let mut local = Vec::<(String, i64, Value)>::new();
                for (key, n) in &nodes {
                    if *key == root_key {
                        continue;
                    }
                    let stripped = strip_disambiguator(key);
                    let Some(target) = aliases.get(stripped) else {
                        continue;
                    };
                    let Some(locked) = n.get("locked") else {
                        continue;
                    };
                    let lm = locked
                        .get("lastModified")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    local.push((target.clone(), lm, locked.clone()));
                }
                Some(local)
            })
            .collect::<Vec<Vec<(String, i64, Value)>>>()
    };

    let mut observations = BTreeMap::<String, Vec<(i64, Value)>>::new();
    for batch in batches {
        for (target, lm, locked) in batch {
            observations.entry(target).or_default().push((lm, locked));
        }
    }

    let mut compare_cache = HashMap::new();
    for (target, mut obs) in observations {
        if let Some(current) = lock.get(&target) {
            let lm = lock::Node::from(current)
                .last_modified()
                .and_then(|lm| i64::try_from(lm).ok())
                .unwrap_or(0);
            obs.insert(0, (lm, current.clone()));
        }
        if let Some(winner) = choose_lock_observation(obs, |base, head| {
            compare_locked_nodes(&mut compare_cache, base, head)
        }) && lock.get(&target) != Some(&winner)
        {
            lock.insert(target, winner);
            changed = true;
        }
    }

    changed
}

pub(in crate::commands) fn choose_lock_observation(
    observations: Vec<(i64, Value)>,
    mut compare: impl FnMut(&Value, &Value) -> Option<CompareStatus>,
) -> Option<Value> {
    let mut iter = observations.into_iter();
    let mut winner = iter.next()?;
    for candidate in iter {
        match compare(&winner.1, &candidate.1) {
            Some(CompareStatus::Ahead) => winner = candidate,
            Some(CompareStatus::Behind | CompareStatus::Identical) => {},
            Some(CompareStatus::Diverged) | None => {
                if candidate.0 > winner.0 {
                    winner = candidate;
                }
            },
        }
    }
    Some(winner.1)
}

fn compare_locked_nodes(
    cache: &mut HashMap<(String, String, String, String), Option<CompareStatus>>,
    base: &Value,
    head: &Value,
) -> Option<CompareStatus> {
    let (owner, repo, base_rev, head_rev) = github_compare_parts(base, head)?;
    if base_rev == head_rev {
        return Some(CompareStatus::Identical);
    }
    let key = (owner, repo, base_rev, head_rev);
    if let Some(cached) = cache.get(&key) {
        return *cached;
    }
    let status = fetch::compare_status(&key.0, &key.1, &key.2, &key.3)
        .ok()
        .flatten();
    cache.insert(key, status);
    status
}

fn github_compare_parts(base: &Value, head: &Value) -> Option<(String, String, String, String)> {
    let (base_node, head_node) = (lock::Node::from(base), lock::Node::from(head));
    if base_node.kind()? != "github" || head_node.kind()? != "github" {
        return None;
    }
    let base_owner = base.get("owner")?.as_str()?;
    let base_repo = base.get("repo")?.as_str()?;
    let head_owner = head.get("owner")?.as_str()?;
    let head_repo = head.get("repo")?.as_str()?;
    if !base_owner.eq_ignore_ascii_case(head_owner) || !base_repo.eq_ignore_ascii_case(head_repo) {
        return None;
    }
    Some((
        base_owner.to_owned(),
        base_repo.to_owned(),
        base_node.rev()?.to_owned(),
        head_node.rev()?.to_owned(),
    ))
}

pub fn look(names: &[String], verbose: bool) -> Result<()> {
    const LOG_LIMIT: usize = 5;

    let project = Project::discover();
    let doc = project.load_pins()?;
    let shorturls = pins::shorturls(&doc);
    let all = pins::inputs(&doc)?;
    if all.is_empty() {
        println!(
            "no pins in {}; add one with `tack add <name> <url>`",
            project.pins_path().display()
        );
        return Ok(());
    }
    let selected = select(&all, names);
    if selected.is_empty() {
        return Ok(());
    }
    let lk = project.load_lock()?;

    let display = Display::new(selected.iter().map(|inp| inp.name.clone()).collect());
    let logs: Vec<Mutex<Option<fetch::CommitLog>>> = iter::repeat_with(|| Mutex::new(None))
        .take(selected.len())
        .collect();

    selected.par_iter().enumerate().for_each(|(i, inp)| {
        if inp.pin_type == PinType::Fixed {
            display.set(
                i,
                PinStatus::Skipped("fixed pin, run `tack update` to verify".into()),
            );
            return;
        }
        display.set(i, PinStatus::Fetching);
        let expanded = shorturl::expand(&inp.url, &shorturls);
        let source = match expanded.parse::<Source>() {
            Ok(source) => source,
            Err(err) => {
                display.set(i, PinStatus::Failed(format!("{err:#}")));
                return;
            },
        };
        let old = lk
            .get(&inp.name)
            .and_then(|n| lock::Node::from(n).rev())
            .map(str::to_owned);
        match fetch::current_rev_compared(&source, old.as_deref()) {
            Ok(current) if old.as_deref() == Some(current.rev.as_str()) => {
                display.set(i, PinStatus::NoChange);
            },
            Ok(current) => {
                display.set(i, PinStatus::Updated {
                    old:        old.as_deref().map_or_else(|| "NEW".into(), render::short),
                    new:        render::short(&current.rev),
                    comparison: current.comparison,
                });
                if verbose
                    && let Some(old_rev) = old.as_deref()
                    && let Ok(Some(log)) =
                        fetch::commits_between(&source, old_rev, &current.rev, LOG_LIMIT)
                {
                    *logs[i].lock().unwrap() = Some(log);
                }
            },
            Err(err) => display.set(i, PinStatus::Failed(format!("{err:#}"))),
        }
    });

    if verbose {
        let collected = logs
            .into_iter()
            .map(|mutex| mutex.into_inner().unwrap())
            .collect::<Vec<_>>();
        display.finish_verbose(&collected);
    } else {
        display.finish();
    }
    Ok(())
}
