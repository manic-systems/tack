// SPDX-License-Identifier: EUPL-1.2

//! verbatim snapshots of `pins.toml`, `pins.lock.json`, and the resolver for
//! `tack undo`/`redo`. those three files fully determine tack's state, so
//! snapshotting their exact bytes is an exact undo. history is an editor-style
//! list of states plus a cursor pointing at the live one.

use std::{
    collections::{
        HashSet,
        hash_map::DefaultHasher,
    },
    env,
    fs,
    hash::{
        Hash as _,
        Hasher as _,
    },
    path::{
        Path,
        PathBuf,
    },
    process,
    time::{
        SystemTime,
        UNIX_EPOCH,
    },
};

use anyhow::Result;
use etcetera::{
    BaseStrategy as _,
    choose_base_strategy,
};
use serde_json::{
    Value,
    json,
};

use crate::project::{
    self,
    Project,
};

const MAX_LEVELS: usize = 20;
const MAX_AGE: u64 = 30 * 24 * 60 * 60;

/// verbatim bytes of the three state files: `(pins.toml, pins.lock.json,
/// resolver)`.
pub type State = (Option<String>, Option<String>, Option<String>);

/// the state of the bytes of all three files, plus the command that produced it
/// and when
#[derive(Default)]
struct Entry {
    label:    String,
    ts:       u64,
    toml:     Option<String>,
    lock:     Option<String>,
    resolver: Option<String>,
}

impl Entry {
    fn matches(&self, state: &State) -> bool {
        self.toml == state.0 && self.lock == state.1 && self.resolver == state.2
    }
}

struct History {
    cursor:  usize,
    entries: Vec<Entry>,
}

/// a label + timestamp pair
pub struct Row {
    pub label: String,
    pub ts:    u64,
}

/// the post-move state of the history
pub struct View {
    pub cursor: usize,
    pub rows:   Vec<Row>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| dur.as_secs())
}

/// read the current on-disk state of all three files
pub fn snapshot(project: &Project) -> State {
    let toml = fs::read_to_string(project.pins_path()).ok();
    let lock = fs::read_to_string(project.lock_path()).ok();
    let resolver = fs::read_to_string(project.resolver_path()).ok();
    (toml, lock, resolver)
}

/// record a `pre -> post` transition under `label`. a no-op appends nothing,
/// and an edit made outside tack is captured as `(edited)` first (returning
/// whether it was). errors are swallowed so recording never breaks a command.
pub fn record(store: &Path, label: &str, pre: State, post: State) -> bool {
    record_inner(store, label, pre, post).unwrap_or(false)
}

fn record_inner(store: &Path, label: &str, pre: State, post: State) -> Result<bool> {
    // a command that left both files untouched is not worth an entry
    if pre == post {
        return Ok(false);
    }
    let ts = now();
    let mut history = load(store);

    let captured_edit = if history.entries.is_empty() {
        history.entries.push(Entry {
            label: "(initial)".to_owned(),
            ts,
            toml: pre.0,
            lock: pre.1,
            resolver: pre.2,
        });
        history.cursor = 0;
        false
    } else {
        capture_external(&mut history, &pre, ts)
    };

    // a fresh mutation supersedes any redo branch
    history.entries.truncate(history.cursor + 1);
    // entries[cursor] now mirrors the on-disk pre-state, so append the result
    if !history.entries[history.cursor].matches(&post) {
        history.entries.push(Entry {
            label: label.to_owned(),
            ts,
            toml: post.0,
            lock: post.1,
            resolver: post.2,
        });
        history.cursor += 1;
    }

    gc(&mut history, ts);
    save(store, &history)?;
    Ok(captured_edit)
}

/// capture the live `state` as an `(edited)` entry when it has diverged from
/// the model (e.g. the user manually edited it), so undo/redo never drop it,
/// truncating the redo branch like a fresh mutation.
fn capture_external(history: &mut History, state: &State, ts: u64) -> bool {
    if history.entries.is_empty() {
        return false;
    }
    if history.entries[history.cursor].matches(state) {
        return false;
    }
    // the live edit becomes the new tip, dropping any redo branch
    history.entries.truncate(history.cursor + 1);
    history.entries.push(Entry {
        label: "(edited)".to_owned(),
        ts,
        toml: state.0.clone(),
        lock: state.1.clone(),
        resolver: state.2.clone(),
    });
    history.cursor += 1;
    true
}

/// prune oldest entries first. two caps (count, age), both refusing to drop the
/// live state or anything redoable
fn gc(history: &mut History, now: u64) {
    while history.entries.len() > MAX_LEVELS && history.cursor > 0 {
        history.entries.remove(0);
        history.cursor -= 1;
    }
    while history.cursor > 0 && now.saturating_sub(history.entries[0].ts) > MAX_AGE {
        history.entries.remove(0);
        history.cursor -= 1;
    }
}

pub fn undo(project: &Project, store: &Path) -> Result<Option<View>> {
    let mut history = load(store);
    // capture any live edit before moving, so undo never drops it
    let captured = capture_external(&mut history, &snapshot(project), now());
    if !captured && history.cursor == 0 {
        return Ok(None);
    }
    if history.cursor > 0 {
        history.cursor -= 1;
        restore(project, &history.entries[history.cursor])?;
    }
    save(store, &history)?;
    Ok(Some(view(&history)))
}

pub fn redo(project: &Project, store: &Path) -> Result<Option<View>> {
    let mut history = load(store);
    // a live edit voids the redo future, so capture it, then report nothing
    if capture_external(&mut history, &snapshot(project), now()) {
        save(store, &history)?;
        return Ok(None);
    }
    if history.entries.is_empty() || history.cursor + 1 >= history.entries.len() {
        return Ok(None);
    }
    history.cursor += 1;
    restore(project, &history.entries[history.cursor])?;
    save(store, &history)?;
    Ok(Some(view(&history)))
}

pub fn list(store: &Path) -> Option<View> {
    let history = load(store);
    if history.entries.is_empty() {
        return None;
    }
    Some(view(&history))
}

fn view(history: &History) -> View {
    View {
        cursor: history.cursor,
        rows:   history
            .entries
            .iter()
            .map(|entry| {
                Row {
                    label: entry.label.clone(),
                    ts:    entry.ts,
                }
            })
            .collect(),
    }
}

/// rewrite all three files from `entry` as one transaction. if any write fails,
/// every file is rolled back to where it started. a [`None`] field means the
/// file was absent in that state and is removed.
fn restore(project: &Project, entry: &Entry) -> Result<()> {
    let specs = [
        (project.pins_path(), entry.toml.as_deref()),
        (project.lock_path(), entry.lock.as_deref()),
        (project.resolver_path(), entry.resolver.as_deref()),
    ];
    let tag = format!(
        "{}.{}",
        process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|dur| dur.as_nanos())
            .unwrap_or_default()
    );
    let mut tx = RestoreTx::new();
    for (path, content) in specs {
        if let Err(err) = tx.stage(&path, content, &tag) {
            tx.cleanup_staged();
            return Err(err);
        }
    }
    match tx.commit() {
        Ok(()) => Ok(()),
        Err(err) => {
            tx.rollback();
            Err(err)
        },
    }
}

/// one file in a [`RestoreTx`]: its staged replacement (`tmp`, [`None`] to
/// remove), a backup of the prior bytes, and how far the commit got
struct RestoreStep {
    path:      PathBuf,
    tmp:       Option<PathBuf>,
    backup:    Option<PathBuf>,
    backed_up: bool,
    installed: bool,
}

/// stage every file as a sibling temp, then commit via renames so a mid-write
/// failure leaves the tree untouched or rolled back
struct RestoreTx {
    steps: Vec<RestoreStep>,
}

impl RestoreTx {
    const fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// write `content` to a sibling temp, or stage a removal when [`None`]
    fn stage(&mut self, path: &Path, content: Option<&str>, tag: &str) -> Result<()> {
        let tmp = if let Some(text) = content {
            let tmp = temp_path(path, tag, "tmp");
            if let Err(err) = fs::write(&tmp, text) {
                let _ = fs::remove_file(&tmp);
                return Err(err.into());
            }
            Some(tmp)
        } else {
            None
        };
        self.steps.push(RestoreStep {
            path: path.to_owned(),
            tmp,
            backup: Some(temp_path(path, tag, "bak")),
            backed_up: false,
            installed: false,
        });
        Ok(())
    }

    /// back up each existing file, swap in its staged temp, then drop backups
    fn commit(&mut self) -> Result<()> {
        for step in &mut self.steps {
            if step.path.exists() {
                let backup = step.backup.as_ref().expect("backup path");
                if backup.exists() {
                    fs::remove_file(backup)?;
                }
                fs::rename(&step.path, backup)?;
                step.backed_up = true;
            }
            if let Some(tmp) = step.tmp.as_ref() {
                fs::rename(tmp, &step.path)?;
                step.installed = true;
            }
            step.tmp = None;
        }
        for step in &mut self.steps {
            if let Some(backup) = step.backup.take()
                && backup.exists()
            {
                let _ = fs::remove_file(backup);
            }
        }
        Ok(())
    }

    /// undo a partial commit by dropping installed files and restoring backups,
    /// newest step first
    fn rollback(&mut self) {
        for step in self.steps.iter_mut().rev() {
            if step.installed && step.path.exists() {
                let _ = fs::remove_file(&step.path);
            }
            if let Some(backup) = step.backup.take()
                && step.backed_up
                && backup.exists()
            {
                let _ = fs::rename(backup, &step.path);
            }
            if let Some(tmp) = step.tmp.take()
                && tmp.exists()
            {
                let _ = fs::remove_file(tmp);
            }
        }
    }

    /// drop temps staged before a failure that aborted ahead of commit
    fn cleanup_staged(&mut self) {
        for step in &mut self.steps {
            if let Some(tmp) = step.tmp.take()
                && tmp.exists()
            {
                let _ = fs::remove_file(tmp);
            }
        }
    }
}

fn temp_path(path: &Path, tag: &str, kind: &str) -> PathBuf {
    let mut tmp_str = path.as_os_str().to_owned();
    tmp_str.push(format!(".undo-{tag}.{kind}"));
    PathBuf::from(tmp_str)
}

/// undo-history dir for `project`, under the XDG state dir so snapshots stay
/// out of the repo
pub fn store_dir(project: &Project) -> PathBuf {
    // XDG state dir on Linux, falling back to the data dir where there is no
    // state dir (e.g. on macOS, Windows)
    let base = choose_base_strategy().map_or_else(
        |_| PathBuf::from(".tack-state"),
        |dirs| dirs.state_dir().unwrap_or_else(|| dirs.data_dir()),
    );
    base.join("tack").join(project_key(project.dir()))
}

fn project_key(project: &Path) -> String {
    let abs = if project.is_absolute() {
        project.to_path_buf()
    } else {
        env::current_dir().map_or_else(|_| project.to_path_buf(), |cwd| cwd.join(project))
    };
    content_key(&abs.to_string_lossy())
}

/// 128-bit hex digest of `text`, recomputed every save, so a hasher change
/// across toolchains just rewrites the snapshot files once.
fn content_key(text: &str) -> String {
    let mut hi = DefaultHasher::new();
    text.hash(&mut hi);
    let mut lo = DefaultHasher::new();
    0x9E37_79B9_7F4A_7C15_u64.hash(&mut lo);
    text.hash(&mut lo);
    format!("{:016x}{:016x}", hi.finish(), lo.finish())
}

fn manifest_path(store: &Path) -> PathBuf {
    store.join("history.json")
}

fn snapshots_dir(store: &Path) -> PathBuf {
    store.join("snapshots")
}

fn load(store: &Path) -> History {
    let Ok(raw) = fs::read_to_string(manifest_path(store)) else {
        return empty();
    };
    let Ok(json) = serde_json::from_str::<Value>(&raw) else {
        return empty();
    };
    let Some(arr) = json.get("entries").and_then(Value::as_array) else {
        return empty();
    };
    let snaps = snapshots_dir(store);
    let mut entries = Vec::with_capacity(arr.len());
    for value in arr {
        // an unreadable ref means a corrupt store, so start fresh
        let Some(entry) = parse_entry(&snaps, value) else {
            return empty();
        };
        entries.push(entry);
    }
    let cursor = json
        .get("cursor")
        .and_then(Value::as_u64)
        .and_then(|num| usize::try_from(num).ok())
        .unwrap_or(0);

    // clamp a possibly-corrupt cursor into range
    let clamped = cursor.min(entries.len().saturating_sub(1));
    History {
        cursor: clamped,
        entries,
    }
}

const fn empty() -> History {
    History {
        cursor:  0,
        entries: Vec::new(),
    }
}

/// read one entry, resolving each file ref to its bytes. a missing key or null
/// means the file was absent. an unreadable ref is corruption ([`None`])
fn parse_entry(snaps: &Path, value: &Value) -> Option<Entry> {
    let resolve = |key: &str| -> Option<Option<String>> {
        let Some(field) = value.get(key) else {
            return Some(None);
        };
        if field.is_null() {
            return Some(None);
        }
        field
            .as_str()
            .and_then(|name| fs::read_to_string(snaps.join(name)).ok())
            .map(Some)
    };
    Some(Entry {
        label:    value
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        ts:       value.get("ts").and_then(Value::as_u64).unwrap_or(0),
        toml:     resolve("toml")?,
        lock:     resolve("lock")?,
        resolver: resolve("resolver")?,
    })
}

fn save(store: &Path, history: &History) -> Result<()> {
    let snaps = snapshots_dir(store);
    fs::create_dir_all(&snaps)?;

    // content-addressed files, written only when absent, so a record writes
    // just its own new snapshot instead of the whole history
    let mut referenced = HashSet::new();
    let mut entries = Vec::with_capacity(history.entries.len());
    for entry in &history.entries {
        entries.push(json!({
            "label": entry.label,
            "ts": entry.ts,
            "toml": persist(&snaps, entry.toml.as_deref(), &mut referenced)?,
            "lock": persist(&snaps, entry.lock.as_deref(), &mut referenced)?,
            "resolver": persist(&snaps, entry.resolver.as_deref(), &mut referenced)?,
        }));
    }
    let doc = json!({
        "cursor": history.cursor,
        "entries": entries,
    });
    let mut json = serde_json::to_string_pretty(&doc)?;
    json.push('\n');

    let path = manifest_path(store);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    project::write_atomic(&path, &json)?;
    sweep(&snaps, &referenced);
    Ok(())
}

/// write `content` to a hash-named file and return the name.
fn persist(snaps: &Path, content: Option<&str>, referenced: &mut HashSet<String>) -> Result<Value> {
    let Some(text) = content else {
        return Ok(Value::Null);
    };
    let name = content_key(text);
    let path = snaps.join(&name);
    if !path.exists() {
        project::write_atomic(&path, text)?;
    }
    referenced.insert(name.clone());
    Ok(Value::String(name))
}

/// drop snapshot files the manifest no longer references, after gc or a
/// truncated redo branch. this is best-effort
fn sweep(snaps: &Path, referenced: &HashSet<String>) {
    let Ok(read) = fs::read_dir(snaps) else {
        return;
    };
    for entry in read.flatten() {
        let keep = entry
            .file_name()
            .to_str()
            .is_some_and(|name| referenced.contains(name));
        if !keep {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// `just now`, `2m ago`, `1h ago`, `3d ago` from two epochs.
pub fn rel_time(now: u64, ts: u64) -> String {
    let delta = now.saturating_sub(ts);
    if delta < 60 {
        "just now".to_owned()
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        Entry,
        History,
        MAX_AGE,
        MAX_LEVELS,
        gc,
        list,
        record,
        redo,
        snapshot,
        undo,
    };
    use crate::project::Project;

    fn write(project: &Project, toml: &str) {
        fs::write(project.pins_path(), toml).unwrap();
        fs::write(project.lock_path(), "{}\n").unwrap();
    }

    fn read(project: &Project) -> String {
        fs::read_to_string(project.pins_path()).unwrap()
    }

    /// stand in for a recorded mutating command: snapshot, mutate, snapshot,
    /// record. returns whether an external edit was captured.
    fn run(project: &Project, label: &str, toml: &str) -> bool {
        let pre = snapshot(project);
        write(project, toml);
        let post = snapshot(project);
        record(project.dir(), label, pre, post)
    }

    #[test]
    fn undo_then_redo_round_trips_state() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let project = Project::at(dir.to_path_buf());
        write(&project, "v1\n");
        run(&project, "a", "v2\n");
        run(&project, "b", "v3\n");
        assert_eq!(read(&project), "v3\n");

        undo(&project, dir).unwrap();
        assert_eq!(read(&project), "v2\n");
        undo(&project, dir).unwrap();
        assert_eq!(read(&project), "v1\n");
        assert!(undo(&project, dir).unwrap().is_none()); // at the initial state
        assert_eq!(read(&project), "v1\n");

        redo(&project, dir).unwrap();
        assert_eq!(read(&project), "v2\n");
        redo(&project, dir).unwrap();
        assert_eq!(read(&project), "v3\n");
        assert!(redo(&project, dir).unwrap().is_none());
    }

    #[test]
    fn undo_captures_external_edit_rather_than_discarding() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let project = Project::at(dir.to_path_buf());
        write(&project, "v1\n");
        run(&project, "a", "v2\n");

        // user hand-edits pins.toml outside tack, then undoes
        write(&project, "manual\n");
        undo(&project, dir).unwrap();
        assert_eq!(read(&project), "v2\n"); // stepped back to the recorded state

        // the manual edit must not be lost
        redo(&project, dir).unwrap();
        assert_eq!(read(&project), "manual\n");
        assert!(
            list(dir)
                .unwrap()
                .rows
                .iter()
                .any(|row| row.label == "(edited)")
        );
    }

    #[test]
    fn record_flags_an_external_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let project = Project::at(dir.to_path_buf());
        write(&project, "v1\n");
        assert!(!run(&project, "a", "v2\n")); // nothing diverged yet

        write(&project, "manual\n"); // unrecorded edit
        assert!(run(&project, "b", "v3\n")); // recorder notices and captures it
    }

    #[test]
    fn new_mutation_truncates_redo_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let project = Project::at(dir.to_path_buf());
        write(&project, "v1\n");
        run(&project, "a", "v2\n");
        run(&project, "b", "v3\n");
        undo(&project, dir).unwrap(); // back to v2 with v3 redoable
        assert_eq!(read(&project), "v2\n");

        run(&project, "c", "v4\n"); // forks a new branch
        assert_eq!(read(&project), "v4\n");
        assert!(redo(&project, dir).unwrap().is_none()); // the v3 future is gone
    }

    #[test]
    fn gc_caps_undo_depth_to_max_levels() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let project = Project::at(dir.to_path_buf());
        write(&project, "s0\n");
        for i in 1..=MAX_LEVELS + 5 {
            run(&project, &format!("m{i}"), &format!("s{i}\n"));
        }
        assert!(list(dir).unwrap().rows.len() <= MAX_LEVELS);
        assert_eq!(read(&project), format!("s{}\n", MAX_LEVELS + 5));
    }

    #[test]
    fn gc_drops_aged_undo_entries_but_keeps_live() {
        let now = MAX_AGE + 1000;
        let aged = |label: &str| {
            Entry {
                label: label.to_owned(),
                ..Default::default()
            }
        };
        let mut history = History {
            cursor:  3,
            entries: vec![aged("0"), aged("1"), aged("2"), Entry {
                label: "live".to_owned(),
                ts: now,
                ..Default::default()
            }],
        };
        gc(&mut history, now);

        // the three aged undo entries are pruned, but the live state survives
        assert_eq!(history.entries.len(), 1);
        assert_eq!(history.cursor, 0);
        assert_eq!(history.entries[0].label, "live");
    }

    #[test]
    fn undo_reverts_the_resolver_too() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let project = Project::at(dir.to_path_buf());
        let resolver = project.resolver_path();
        write(&project, "v1\n");
        fs::write(&resolver, "resolver-v1\n").unwrap();
        let pre = snapshot(&project);

        // a resolver-only change, e.g. `tack init --resolver`
        fs::write(&resolver, "resolver-v2\n").unwrap();
        let post = snapshot(&project);
        assert!(pre != post); // the 3-file snapshot notices the resolver change
        record(dir, "init --resolver", pre, post);

        undo(&project, dir).unwrap();
        assert_eq!(fs::read_to_string(&resolver).unwrap(), "resolver-v1\n");
    }
}
