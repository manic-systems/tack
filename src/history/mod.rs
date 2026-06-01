// SPDX-License-Identifier: EUPL-1.2

//! verbatim snapshots of `pins.toml`, `pins.lock.json`, and the resolver
//! those three files fully determine tack's state
//! history is an editor-style list of states plus a cursor pointing at the live
//! one

use std::time::{
    SystemTime,
    UNIX_EPOCH,
};

mod restore;
mod snapshot;
mod store;

pub use snapshot::Snapshot;
pub use store::HistoryStore;

const MAX_LEVELS: usize = 20;
const MAX_AGE: u64 = 30 * 24 * 60 * 60;

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
        state.matches_entry(self)
    }
}

struct History {
    cursor:  usize,
    entries: Vec<Entry>,
}

impl History {
    const fn empty() -> Self {
        Self {
            cursor:  0,
            entries: Vec::new(),
        }
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
    history
        .entries
        .push(state.clone().into_entry("(edited)".to_owned(), ts));
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
        gc,
        snapshot::content_key,
    };
    use crate::project::Project;

    fn store(dir: &Path) -> HistoryStore {
        HistoryStore::at(dir.to_path_buf())
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
        store.record(label, pre, post).unwrap()
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
    fn record_run_surfaces_history_write_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let project = Project::at(dir.to_path_buf());
        write(&project, "v1\n");
        let store_path = dir.join("history-file");
        fs::write(&store_path, "not a dir").unwrap();
        let store = store(&store_path);

        let outcome = store.record_run(&project, "a", || {
            write(&project, "v2\n");
            Ok(())
        });

        outcome.result.unwrap();
        assert!(outcome.history_error.is_some());
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
        store.record("init --resolver", pre, post).unwrap();

        store.undo(&project).unwrap();
        assert_eq!(fs::read_to_string(&resolver).unwrap(), "resolver-v1\n");
    }
}
