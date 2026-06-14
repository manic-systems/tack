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

#[cfg(test)] mod tests;
