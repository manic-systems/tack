// SPDX-License-Identifier: EUPL-1.2

use super::{
    Project,
    Result,
    history,
    render,
};

pub fn undo(list: bool) -> Result<()> {
    let project = Project::discover();
    let store = history::store_dir(&project);
    if list {
        match history::list(&store) {
            Some(view) => render::render(&view, 0, view.rows.len().saturating_sub(1)),
            None => println!("no history"),
        }
        return Ok(());
    }
    match history::undo(&project, &store)? {
        Some(view) => render::render_window(&view),
        None => println!("nothing to undo"),
    }
    Ok(())
}

pub fn redo() -> Result<()> {
    let project = Project::discover();
    let store = history::store_dir(&project);
    match history::redo(&project, &store)? {
        Some(view) => render::render_window(&view),
        None => println!("nothing to redo"),
    }
    Ok(())
}
