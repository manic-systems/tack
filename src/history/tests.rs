// SPDX-License-Identifier: EUPL-1.2

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
