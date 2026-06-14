// SPDX-License-Identifier: EUPL-1.2

//! undo history over tack state files

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

pub struct Row {
    pub label: String,
    pub ts:    u64,
}

pub struct View {
    pub cursor: usize,
    pub rows:   Vec<Row>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| dur.as_secs())
}

/// keep external edits before moving the undo cursor
fn capture_external(history: &mut History, state: &Snapshot, ts: u64) -> bool {
    if history.entries.is_empty() {
        return false;
    }
    if history.entries[history.cursor].matches(state) {
        return false;
    }
    history.entries.truncate(history.cursor + 1);
    history
        .entries
        .push(state.clone().into_entry("(edited)".to_owned(), ts));
    history.cursor += 1;
    true
}

/// prune only undoable history
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
        HistoryStore,
        Snapshot,
    };
    use crate::project::Project;

    fn store(dir: &Path) -> HistoryStore {
        HistoryStore::at(dir.to_path_buf())
    }

    fn write(project: &Project, toml: &str) {
        fs::write(project.pins_path(), toml).unwrap();
        fs::write(project.lock_path(), "{}\n").unwrap();
    }

    fn read(project: &Project) -> String {
        fs::read_to_string(project.pins_path()).unwrap()
    }

    fn run(project: &Project, store: &HistoryStore, label: &str, toml: &str) {
        let pre = Snapshot::capture(project);
        write(project, toml);
        let post = Snapshot::capture(project);
        store.record(label, pre, post).unwrap();
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

        store.undo(&project).unwrap();
        assert_eq!(read(&project), "v2\n");
        store.undo(&project).unwrap();
        assert_eq!(read(&project), "v1\n");
        store.redo(&project).unwrap();
        assert_eq!(read(&project), "v2\n");
        store.redo(&project).unwrap();
        assert_eq!(read(&project), "v3\n");
    }

    #[test]
    fn undo_preserves_external_edits_as_redo_state() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let project = Project::at(dir.to_path_buf());
        let store = store(dir);
        write(&project, "v1\n");
        run(&project, &store, "a", "v2\n");

        write(&project, "manual\n");
        store.undo(&project).unwrap();
        assert_eq!(read(&project), "v2\n");

        store.redo(&project).unwrap();
        assert_eq!(read(&project), "manual\n");
    }
}
