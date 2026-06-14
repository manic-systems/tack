// SPDX-License-Identifier: EUPL-1.2

use std::{
    fs,
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

use eyre::Result;

use super::Entry;
use crate::project::Project;

/// restore all state files as one transaction
pub(super) fn restore(project: &Project, entry: &Entry) -> Result<()> {
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

struct RestoreStep {
    path:      PathBuf,
    tmp:       Option<PathBuf>,
    backup:    Option<PathBuf>,
    backed_up: bool,
    installed: bool,
}

struct RestoreTx {
    steps: Vec<RestoreStep>,
}

impl RestoreTx {
    const fn new() -> Self {
        Self { steps: Vec::new() }
    }

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

    /// undo a partial commit
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
