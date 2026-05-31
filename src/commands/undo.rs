// SPDX-License-Identifier: EUPL-1.2

use eyre::Result;

use crate::{
    history,
    project::Project,
    render,
};

pub fn undo(project: &Project, list: bool) -> Result<()> {
    let store = history::HistoryStore::for_project(project);
    if list {
        match store.list() {
            Some(view) => render::render(&view, 0, view.rows.len().saturating_sub(1)),
            None => println!("no history"),
        }
        return Ok(());
    }
    match store.undo(project)? {
        Some(view) => render::render_window(&view),
        None => println!("nothing to undo"),
    }
    Ok(())
}

pub fn redo(project: &Project) -> Result<()> {
    let store = history::HistoryStore::for_project(project);
    match store.redo(project)? {
        Some(view) => render::render_window(&view),
        None => println!("nothing to redo"),
    }
    Ok(())
}
