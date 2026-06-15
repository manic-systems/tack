// SPDX-License-Identifier: EUPL-1.2

use eyre::Result;

use crate::{
    history,
    history::View,
    project::Project,
    render,
};

pub fn undo(project: &Project, list: bool) -> Result<()> {
    if list {
        match history(project) {
            Some(view) => render::render(&view, 0, view.rows.len().saturating_sub(1)),
            None => println!("no history"),
        }
        return Ok(());
    }
    match undo_view(project)? {
        Some(view) => render::render_window(&view),
        None => println!("nothing to undo"),
    }
    Ok(())
}

pub fn redo(project: &Project) -> Result<()> {
    match redo_view(project)? {
        Some(view) => render::render_window(&view),
        None => println!("nothing to redo"),
    }
    Ok(())
}

pub fn history(project: &Project) -> Option<View> {
    let store = history::HistoryStore::for_project(project);
    store.list()
}

pub fn undo_view(project: &Project) -> Result<Option<View>> {
    let store = history::HistoryStore::for_project(project);
    store.undo(project)
}

pub fn redo_view(project: &Project) -> Result<Option<View>> {
    let store = history::HistoryStore::for_project(project);
    store.redo(project)
}
