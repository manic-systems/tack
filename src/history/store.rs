// SPDX-License-Identifier: EUPL-1.2

use std::{
    collections::HashSet,
    env,
    fs,
    path::{
        Path,
        PathBuf,
    },
};

use etcetera::{
    BaseStrategy as _,
    choose_base_strategy,
};
use eyre::Result;

use super::{
    History,
    View,
    capture_external,
    gc,
    now,
    restore::restore,
    snapshot::{
        Snapshot,
        StoredEntry,
        StoredHistory,
        content_key,
        legacy_content_key,
        persist,
        sweep,
    },
    view,
};
use crate::project::{
    self,
    Project,
};

/// undo history for one project
pub struct HistoryStore {
    dir: PathBuf,
}

pub struct RecordedRun {
    pub result:            Result<()>,
    pub captured_external: bool,
    pub history_error:     Option<eyre::Report>,
}

impl HistoryStore {
    #[cfg(test)]
    pub(super) const fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// history dir for project, under the xdg state dir
    pub fn for_project(project: &Project) -> Self {
        Self {
            dir: store_dir(project),
        }
    }

    /// record a pre to post transition under label
    pub fn record(&self, label: &str, pre: Snapshot, post: Snapshot) -> Result<bool> {
        self.record_inner(label, pre, post)
    }

    /// run a mutating command and record the resulting file diff
    pub fn record_run<F>(&self, project: &Project, label: &str, run: F) -> RecordedRun
    where
        F: FnOnce() -> Result<()>,
    {
        let pre = Snapshot::capture(project);
        let result = run();
        let post = Snapshot::capture(project);
        let recorded = self.record(label, pre, post);
        let (captured_external, history_error) = match recorded {
            Ok(captured_external) => (captured_external, None),
            Err(err) => (false, Some(err)),
        };
        RecordedRun {
            result,
            captured_external,
            history_error,
        }
    }

    fn record_inner(&self, label: &str, pre: Snapshot, post: Snapshot) -> Result<bool> {
        if pre == post {
            return Ok(false);
        }
        let ts = now();
        let mut history = self.load();

        let captured_edit = if history.entries.is_empty() {
            history
                .entries
                .push(pre.into_entry("(initial)".to_owned(), ts));
            history.cursor = 0;
            false
        } else {
            capture_external(&mut history, &pre, ts)
        };

        history.entries.truncate(history.cursor + 1);

        if !post.matches_entry(&history.entries[history.cursor]) {
            history.entries.push(post.into_entry(label.to_owned(), ts));
            history.cursor += 1;
        }

        gc(&mut history, ts);
        self.save(&history)?;
        Ok(captured_edit)
    }

    pub fn undo(&self, project: &Project) -> Result<Option<View>> {
        let mut history = self.load();
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
            return History::empty();
        };
        let Ok(stored) = serde_json::from_str::<StoredHistory>(&raw) else {
            return History::empty();
        };
        let (cursor, stored_entries) = stored.into_parts();
        let snaps = self.snapshots_dir();
        let mut entries = Vec::with_capacity(stored_entries.len());
        for value in stored_entries {
            let Some(entry) = value.resolve(&snaps) else {
                return History::empty();
            };
            entries.push(entry);
        }

        let clamped = cursor.min(entries.len().saturating_sub(1));
        History {
            cursor: clamped,
            entries,
        }
    }

    fn save(&self, history: &History) -> Result<()> {
        let snaps = self.snapshots_dir();
        fs::create_dir_all(&snaps)?;

        let mut referenced = HashSet::new();
        let mut entries = Vec::with_capacity(history.entries.len());
        for entry in &history.entries {
            entries.push(StoredEntry::new(
                entry.label.clone(),
                entry.ts,
                persist(&snaps, entry.toml.as_deref(), &mut referenced)?,
                persist(&snaps, entry.lock.as_deref(), &mut referenced)?,
                persist(&snaps, entry.resolver.as_deref(), &mut referenced)?,
            ));
        }
        let doc = StoredHistory::new(history.cursor, entries);
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

fn store_dir(project: &Project) -> PathBuf {
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
