// SPDX-License-Identifier: EUPL-1.2

//! verbatim snapshots of `pins.toml`, `pins.lock.json`, and the resolver
//! those three files fully determine tack's state
//! history is an editor-style list of states plus a cursor pointing at the live
//! one

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
    result::Result as StdResult,
    time::{
        SystemTime,
        UNIX_EPOCH,
    },
};

use data_encoding::HEXLOWER;
use etcetera::{
    BaseStrategy as _,
    choose_base_strategy,
};
use eyre::Result;
use hmac_sha256::Hash as Sha256;
use serde::{
    Deserialize,
    Deserializer,
    Serialize,
    Serializer,
};

use crate::project::{
    self,
    Project,
};

const MAX_LEVELS: usize = 20;
const MAX_AGE: u64 = 30 * 24 * 60 * 60;

/// verbatim bytes of the three state files that determine tack's state
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    toml:     Option<String>,
    lock:     Option<String>,
    resolver: Option<String>,
}

impl Snapshot {
    /// read the current on-disk state of all three files
    pub fn capture(project: &Project) -> Self {
        Self {
            toml:     fs::read_to_string(project.pins_path()).ok(),
            lock:     fs::read_to_string(project.lock_path()).ok(),
            resolver: fs::read_to_string(project.resolver_path()).ok(),
        }
    }
}

/// undo-history store for one project
pub struct HistoryStore {
    dir: PathBuf,
}

pub struct RecordedRun {
    pub result:            Result<()>,
    pub captured_external: bool,
}

impl HistoryStore {
    /// undo-history dir for `project`, under the xdg state dir so snapshots
    /// stay out of the repo
    pub fn for_project(project: &Project) -> Self {
        Self {
            dir: store_dir(project),
        }
    }

    /// record a `pre -> post` transition under `label`
    /// no-op writes nothing, outside edits are captured first
    pub fn record(&self, label: &str, pre: Snapshot, post: Snapshot) -> bool {
        self.record_inner(label, pre, post).unwrap_or(false)
    }

    /// run a mutating command and record the resulting file diff
    /// failures are recorded too because partial writes are still recoverable
    pub fn record_run<F>(&self, project: &Project, label: &str, run: F) -> RecordedRun
    where
        F: FnOnce() -> Result<()>,
    {
        let pre = Snapshot::capture(project);
        let result = run();
        let post = Snapshot::capture(project);
        RecordedRun {
            result,
            captured_external: self.record(label, pre, post),
        }
    }

    fn record_inner(&self, label: &str, pre: Snapshot, post: Snapshot) -> Result<bool> {
        // a command that left both files untouched is not worth an entry
        if pre == post {
            return Ok(false);
        }
        let ts = now();
        let mut history = self.load();

        let captured_edit = if history.entries.is_empty() {
            history.entries.push(Entry {
                label: "(initial)".to_owned(),
                ts,
                toml: pre.toml,
                lock: pre.lock,
                resolver: pre.resolver,
            });
            history.cursor = 0;
            false
        } else {
            capture_external(&mut history, &pre, ts)
        };

        // a fresh mutation supersedes any redo branch
        history.entries.truncate(history.cursor + 1);

        // entries[cursor] mirrors the on-disk pre-state, so append the result
        if !history.entries[history.cursor].matches(&post) {
            history.entries.push(Entry {
                label: label.to_owned(),
                ts,
                toml: post.toml,
                lock: post.lock,
                resolver: post.resolver,
            });
            history.cursor += 1;
        }

        gc(&mut history, ts);
        self.save(&history)?;
        Ok(captured_edit)
    }

    pub fn undo(&self, project: &Project) -> Result<Option<View>> {
        let mut history = self.load();
        // capture any live edit before moving, so undo never drops it
        let captured = capture_external(&mut history, &Snapshot::capture(project), now());
        if !captured && history.cursor == 0 {
            return Ok(None);
        }
        if history.cursor > 0 {
            history.cursor -= 1;
            restore(project, &history.entries[history.cursor])?;
        }
        self.save(&history)?;
        Ok(Some(view(&history)))
    }

    pub fn redo(&self, project: &Project) -> Result<Option<View>> {
        let mut history = self.load();
        // a live edit voids the redo future, so capture it, then report nothing
        if capture_external(&mut history, &Snapshot::capture(project), now()) {
            self.save(&history)?;
            return Ok(None);
        }
        if history.entries.is_empty() || history.cursor + 1 >= history.entries.len() {
            return Ok(None);
        }
        history.cursor += 1;
        restore(project, &history.entries[history.cursor])?;
        self.save(&history)?;
        Ok(Some(view(&history)))
    }

    pub fn list(&self) -> Option<View> {
        let history = self.load();
        if history.entries.is_empty() {
            return None;
        }
        Some(view(&history))
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join("history.json")
    }

    fn snapshots_dir(&self) -> PathBuf {
        self.dir.join("snapshots")
    }

    fn load(&self) -> History {
        let Ok(raw) = fs::read_to_string(self.manifest_path()) else {
            return empty();
        };
        let Ok(stored) = serde_json::from_str::<StoredHistory>(&raw) else {
            return empty();
        };
        let snaps = self.snapshots_dir();
        let mut entries = Vec::with_capacity(stored.entries.len());
        for value in stored.entries {
            // an unreadable ref means a corrupt store, so start fresh
            let Some(entry) = value.resolve(&snaps) else {
                return empty();
            };
            entries.push(entry);
        }

        // clamp a possibly-corrupt cursor into range
        let clamped = stored.cursor.min(entries.len().saturating_sub(1));
        History {
            cursor: clamped,
            entries,
        }
    }

    fn save(&self, history: &History) -> Result<()> {
        let snaps = self.snapshots_dir();
        fs::create_dir_all(&snaps)?;

        // content-addressed files, written only when absent, so a record writes
        // just its own new snapshot instead of the whole history
        let mut referenced = HashSet::new();
        let mut entries = Vec::with_capacity(history.entries.len());
        for entry in &history.entries {
            entries.push(StoredEntry {
                label:    entry.label.clone(),
                ts:       entry.ts,
                toml:     persist(&snaps, entry.toml.as_deref(), &mut referenced)?,
                lock:     persist(&snaps, entry.lock.as_deref(), &mut referenced)?,
                resolver: persist(&snaps, entry.resolver.as_deref(), &mut referenced)?,
            });
        }
        let doc = StoredHistory {
            cursor: history.cursor,
            entries,
        };
        let mut json = serde_json::to_string_pretty(&doc)?;
        json.push('\n');

        let path = self.manifest_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        project::write_atomic(&path, &json)?;
        sweep(&snaps, &referenced);
        Ok(())
    }
}

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
    fn matches(&self, state: &Snapshot) -> bool {
        self.toml == state.toml && self.lock == state.lock && self.resolver == state.resolver
    }
}

struct History {
    cursor:  usize,
    entries: Vec<Entry>,
}

#[derive(Deserialize, Serialize)]
struct StoredHistory {
    #[serde(default)]
    cursor:  usize,
    #[serde(default)]
    entries: Vec<StoredEntry>,
}

#[derive(Deserialize, Serialize)]
struct StoredEntry {
    #[serde(default)]
    label:    String,
    #[serde(default)]
    ts:       u64,
    #[serde(default)]
    toml:     SnapshotRef,
    #[serde(default)]
    lock:     SnapshotRef,
    #[serde(default)]
    resolver: SnapshotRef,
}

impl StoredEntry {
    fn resolve(self, snaps: &Path) -> Option<Entry> {
        Some(Entry {
            label:    self.label,
            ts:       self.ts,
            toml:     self.toml.resolve(snaps)?.into_option(),
            lock:     self.lock.resolve(snaps)?.into_option(),
            resolver: self.resolver.resolve(snaps)?.into_option(),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum SnapshotRef {
    #[default]
    Missing,
    Present(String),
}

impl SnapshotRef {
    fn resolve(&self, snaps: &Path) -> Option<SnapshotBytes> {
        match *self {
            Self::Missing => Some(SnapshotBytes::Missing),
            Self::Present(ref name) => {
                fs::read_to_string(snaps.join(name))
                    .ok()
                    .map(SnapshotBytes::Present)
            },
        }
    }
}

enum SnapshotBytes {
    Missing,
    Present(String),
}

impl SnapshotBytes {
    fn into_option(self) -> Option<String> {
        match self {
            Self::Missing => None,
            Self::Present(content) => Some(content),
        }
    }
}

impl Serialize for SnapshotRef {
    fn serialize<S>(&self, serializer: S) -> StdResult<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *self {
            Self::Missing => serializer.serialize_none(),
            Self::Present(ref name) => serializer.serialize_some(name),
        }
    }
}

impl<'de> Deserialize<'de> for SnapshotRef {
    fn deserialize<D>(deserializer: D) -> StdResult<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Option::<String>::deserialize(deserializer)?.map_or(Self::Missing, Self::Present))
    }
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

/// capture a live edit as an `(edited)` entry so undo/redo never drop it
/// this truncates the redo branch like a fresh mutation
fn capture_external(history: &mut History, state: &Snapshot, ts: u64) -> bool {
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
        toml: state.toml.clone(),
        lock: state.lock.clone(),
        resolver: state.resolver.clone(),
    });
    history.cursor += 1;
    true
}

/// prune oldest entries first
/// two caps, count and age, both refusing to drop the
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

/// rewrite all three files from `entry` as one transaction
/// if any write fails, every file is rolled back to where it started
/// a [`None`] field means the file was absent in that state and is removed
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

/// one file in a [`RestoreTx`]
/// staged replacement, prior backup, and how far the commit got
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

fn store_dir(project: &Project) -> PathBuf {
    // xdg state dir on linux, falling back to the data dir where there is no
    // state dir
    let base = choose_base_strategy().map_or_else(
        |_| PathBuf::from(".tack-state"),
        |dirs| dirs.state_dir().unwrap_or_else(|| dirs.data_dir()),
    );
    let root = base.join("tack");
    let stable = root.join(project_key(project.dir()));
    let legacy = root.join(legacy_project_key(project.dir()));
    if !stable.exists() && legacy.exists() && fs::rename(&legacy, &stable).is_err() {
        return legacy;
    }
    stable
}

fn project_key(project: &Path) -> String {
    content_key(&project_identity(project))
}

fn legacy_project_key(project: &Path) -> String {
    legacy_content_key(&project_identity(project))
}

fn project_identity(project: &Path) -> String {
    let abs = if project.is_absolute() {
        project.to_path_buf()
    } else {
        env::current_dir().map_or_else(|_| project.to_path_buf(), |cwd| cwd.join(project))
    };
    abs.to_string_lossy().into_owned()
}

/// stable sha256 hex digest of text
fn content_key(text: &str) -> String {
    HEXLOWER.encode(&Sha256::hash(text.as_bytes()))
}

fn legacy_content_key(text: &str) -> String {
    let mut hi = DefaultHasher::new();
    text.hash(&mut hi);
    let mut lo = DefaultHasher::new();
    0x9E37_79B9_7F4A_7C15_u64.hash(&mut lo);
    text.hash(&mut lo);
    format!("{:016x}{:016x}", hi.finish(), lo.finish())
}

const fn empty() -> History {
    History {
        cursor:  0,
        entries: Vec::new(),
    }
}

/// write `content` to a hash-named file and return the name
fn persist(
    snaps: &Path,
    content: Option<&str>,
    referenced: &mut HashSet<String>,
) -> Result<SnapshotRef> {
    let Some(text) = content else {
        return Ok(SnapshotRef::Missing);
    };
    let name = content_key(text);
    let path = snaps.join(&name);
    if !path.exists() {
        project::write_atomic(&path, text)?;
    }
    referenced.insert(name.clone());
    Ok(SnapshotRef::Present(name))
}

/// drop snapshot files the manifest no longer references, after gc or a
/// truncated redo branch
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

/// `just now`, `2m ago`, `1h ago`, `3d ago` from two epochs
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
    use std::{
        fs,
        path::Path,
    };

    use super::{
        Entry,
        History,
        HistoryStore,
        MAX_AGE,
        MAX_LEVELS,
        Snapshot,
        content_key,
        gc,
    };
    use crate::project::Project;

    fn store(dir: &Path) -> HistoryStore {
        HistoryStore {
            dir: dir.to_path_buf(),
        }
    }

    fn write(project: &Project, toml: &str) {
        fs::write(project.pins_path(), toml).unwrap();
        fs::write(project.lock_path(), "{}\n").unwrap();
    }

    #[test]
    fn content_key_is_stable_sha256_hex() {
        assert_eq!(
            content_key("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn read(project: &Project) -> String {
        fs::read_to_string(project.pins_path()).unwrap()
    }

    /// stand in for a recorded mutating command
    /// returns whether an external edit was captured
    fn run(project: &Project, store: &HistoryStore, label: &str, toml: &str) -> bool {
        let pre = Snapshot::capture(project);
        write(project, toml);
        let post = Snapshot::capture(project);
        store.record(label, pre, post)
    }

    #[test]
    fn undo_then_redo_round_trips_state() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let project = Project::at(dir.to_path_buf());
        let store = store(dir);
        write(&project, "v1\n");
        run(&project, &store, "a", "v2\n");
        run(&project, &store, "b", "v3\n");
        assert_eq!(read(&project), "v3\n");

        store.undo(&project).unwrap();
        assert_eq!(read(&project), "v2\n");
        store.undo(&project).unwrap();
        assert_eq!(read(&project), "v1\n");
        assert!(store.undo(&project).unwrap().is_none()); // at the initial state
        assert_eq!(read(&project), "v1\n");

        store.redo(&project).unwrap();
        assert_eq!(read(&project), "v2\n");
        store.redo(&project).unwrap();
        assert_eq!(read(&project), "v3\n");
        assert!(store.redo(&project).unwrap().is_none());
    }

    #[test]
    fn undo_captures_external_edit_rather_than_discarding() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let project = Project::at(dir.to_path_buf());
        let store = store(dir);
        write(&project, "v1\n");
        run(&project, &store, "a", "v2\n");

        // user hand-edits pins.toml outside tack, then undoes
        write(&project, "manual\n");
        store.undo(&project).unwrap();
        assert_eq!(read(&project), "v2\n"); // stepped back to the recorded state

        // the manual edit must not be lost
        store.redo(&project).unwrap();
        assert_eq!(read(&project), "manual\n");
        assert!(
            store
                .list()
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
        let store = store(dir);
        write(&project, "v1\n");
        assert!(!run(&project, &store, "a", "v2\n")); // nothing diverged yet

        write(&project, "manual\n"); // unrecorded edit
        assert!(run(&project, &store, "b", "v3\n")); // recorder notices and captures it
    }

    #[test]
    fn new_mutation_truncates_redo_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let project = Project::at(dir.to_path_buf());
        let store = store(dir);
        write(&project, "v1\n");
        run(&project, &store, "a", "v2\n");
        run(&project, &store, "b", "v3\n");
        store.undo(&project).unwrap(); // back to v2 with v3 redoable
        assert_eq!(read(&project), "v2\n");

        run(&project, &store, "c", "v4\n"); // forks a new branch
        assert_eq!(read(&project), "v4\n");
        assert!(store.redo(&project).unwrap().is_none()); // the v3 future is gone
    }

    #[test]
    fn gc_caps_undo_depth_to_max_levels() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let project = Project::at(dir.to_path_buf());
        let store = store(dir);
        write(&project, "s0\n");
        for i in 1..=MAX_LEVELS + 5 {
            run(&project, &store, &format!("m{i}"), &format!("s{i}\n"));
        }
        assert!(store.list().unwrap().rows.len() <= MAX_LEVELS);
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
        let store = store(dir);
        let resolver = project.resolver_path();
        write(&project, "v1\n");
        fs::write(&resolver, "resolver-v1\n").unwrap();
        let pre = Snapshot::capture(&project);

        // resolver-only change such as `tack init --resolver`
        fs::write(&resolver, "resolver-v2\n").unwrap();
        let post = Snapshot::capture(&project);
        assert!(pre != post); // the 3-file snapshot notices the resolver change
        store.record("init --resolver", pre, post);

        store.undo(&project).unwrap();
        assert_eq!(fs::read_to_string(&resolver).unwrap(), "resolver-v1\n");
    }
}
