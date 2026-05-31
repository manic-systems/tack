// SPDX-License-Identifier: EUPL-1.2

use std::{
    fmt::Write as _,
    io::{
        self,
        IsTerminal as _,
        Write as _,
    },
    sync::{
        Arc,
        Mutex,
        atomic::{
            AtomicBool,
            Ordering,
        },
    },
    thread::{
        self,
        JoinHandle,
    },
    time::Duration,
};

use crate::fetch::{
    BranchComparison,
    CommitLog,
    CompareStatus,
};

#[derive(Clone)]
pub enum PinStatus {
    Pending,
    Fetching,
    NoChange,
    Updated {
        old:        String,
        new:        String,
        comparison: BranchComparison,
    },
    Drift {
        rev:      String,
        accepted: bool,
    },
    /// fixed-pin identity moved; old + new sha256 short forms
    FixedDrift {
        old:      String,
        new:      String,
        accepted: bool,
    },
    /// pin intentionally skipped with a one-line note
    Skipped(String),
    Failed(String),
}

const FRAMES: [char; 4] = ['/', '-', '\\', '|'];

pub struct Display {
    states: Arc<Mutex<Vec<PinStatus>>>,
    names:  Arc<[String]>,
    stop:   Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    tty:    bool,
}

impl Display {
    pub fn new(initial_names: Vec<String>) -> Self {
        let tty = io::stdout().is_terminal();
        let states = Arc::new(Mutex::new(vec![PinStatus::Pending; initial_names.len()]));
        let names = initial_names.into();
        let stop = Arc::new(AtomicBool::new(false));

        let handle = tty.then(|| {
            let states_for_draw = Arc::clone(&states);
            let names_for_draw = Arc::clone(&names);
            let stop_for_draw = Arc::clone(&stop);
            thread::spawn(move || {
                let mut drawn = false;
                let mut frame = 0;
                while !stop_for_draw.load(Ordering::Relaxed) {
                    draw(
                        &names_for_draw,
                        &states_for_draw.lock().unwrap(),
                        frame,
                        drawn,
                    );
                    drawn = true;
                    frame = frame.wrapping_add(1);
                    thread::sleep(Duration::from_millis(67));
                }
                draw(
                    &names_for_draw,
                    &states_for_draw.lock().unwrap(),
                    frame,
                    drawn,
                );
            })
        });
        Self {
            states,
            names,
            stop,
            handle,
            tty,
        }
    }

    pub fn set(&self, i: usize, status: PinStatus) {
        self.states.lock().unwrap()[i] = status;
    }

    pub fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if !self.tty {
            let states = self.states.lock().unwrap();
            for (name, st) in self.names.iter().zip(states.iter()) {
                if let Some(line) = plain_line(name, st) {
                    println!("{line}");
                }
            }
        }
    }

    /// finish + render the per-pin commit log under each Updated entry
    pub fn finish_verbose(mut self, logs: &[Option<CommitLog>]) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let states = self.states.lock().unwrap();
        let mut out = io::stdout().lock();
        if self.tty {
            // rewind to the first pin row and clear what the spinner drew
            let _ = write!(out, "\x1b[{}A\x1b[J", self.names.len());
            for ((name, status), entry) in self.names.iter().zip(states.iter()).zip(logs.iter()) {
                let _ = writeln!(out, "[{}] {name}{}", glyph(status, 0), suffix(status));
                if matches!(*status, PinStatus::Updated { .. })
                    && let Some(log) = entry.as_ref()
                {
                    let indent = " ".repeat(4 + name.len() + 2);
                    print_log(&mut out, &indent, log);
                }
            }
        } else {
            for ((name, status), entry) in self.names.iter().zip(states.iter()).zip(logs.iter()) {
                if let Some(line) = plain_line(name, status) {
                    let _ = writeln!(out, "{line}");
                }
                if matches!(*status, PinStatus::Updated { .. })
                    && let Some(log) = entry.as_ref()
                {
                    let indent = " ".repeat(name.len() + 2);
                    print_log(&mut out, &indent, log);
                }
            }
        }
        let _ = out.flush();
    }
}

fn short_hash(hash: &str) -> &str {
    hash.get(..7).expect("hash should be at least 7 bytes")
}

fn print_log(out: &mut dyn io::Write, indent: &str, log: &CommitLog) {
    for &(ref hash, ref subject) in &log.fresh {
        let _ = writeln!(out, "{indent}{}    {subject}", short_hash(hash));
    }
    if log.more {
        let _ = writeln!(out, "{indent}...");
    }
    if let Some(&(ref hash, ref subject)) = log.base.as_ref() {
        let _ = writeln!(out, "{indent}{}    {subject}", short_hash(hash));
    }
}

fn draw(names: &[String], states: &[PinStatus], frame: usize, drawn: bool) {
    let mut out = String::new();
    if drawn {
        let _ = write!(out, "\x1b[{}A", names.len());
    }
    for (name, st) in names.iter().zip(states) {
        out.push_str("\x1b[2K");
        let _ = writeln!(out, "[{}] {name}{}", glyph(st, frame), suffix(st));
    }
    let mut so = io::stdout().lock();
    let _ = so.write_all(out.as_bytes());
    let _ = so.flush();
}

fn glyph(st: &PinStatus, frame: usize) -> String {
    let (color, ch) = match *st {
        PinStatus::Fetching => (34_i32, FRAMES[frame % FRAMES.len()]),
        PinStatus::NoChange => (32_i32, '\u{2713}'), // ✓
        PinStatus::Updated { .. } => (33_i32, '*'),
        PinStatus::Drift { accepted: true, .. } | PinStatus::FixedDrift { accepted: true, .. } => {
            (33_i32, '~')
        },
        PinStatus::Drift {
            accepted: false, ..
        }
        | PinStatus::FixedDrift {
            accepted: false, ..
        } => (31_i32, '!'),
        PinStatus::Pending | PinStatus::Skipped(_) => (2_i32, '\u{b7}'), // ·
        PinStatus::Failed(_) => (31_i32, '\u{2717}'),                    // ✗
    };
    format!("\x1b[{color}m{ch}\x1b[0m")
}

fn suffix(st: &PinStatus) -> String {
    match *st {
        PinStatus::Updated {
            ref old,
            ref new,
            comparison,
        } => {
            format!("  {old} -> {new}{}", comparison_suffix(comparison))
        },
        PinStatus::Drift {
            ref rev,
            accepted: false,
        } => {
            format!("  DRIFT: rev {rev} unchanged but content differs (lock kept)")
        },
        PinStatus::Drift {
            ref rev,
            accepted: true,
        } => {
            format!("  DRIFT: rev {rev} content changed, relocked (--accept)")
        },
        PinStatus::FixedDrift {
            ref old,
            ref new,
            accepted: false,
        } => {
            format!(
                "  DRIFT: fixed pin sha256 changed {old} -> {new} (lock kept; --accept to relock)"
            )
        },
        PinStatus::FixedDrift {
            ref old,
            ref new,
            accepted: true,
        } => {
            format!("  DRIFT: fixed pin sha256 changed {old} -> {new}, relocked (--accept)")
        },
        PinStatus::Skipped(ref note) => format!("  {note}"),
        PinStatus::Failed(ref msg) => format!("  {msg}"),
        PinStatus::Pending | PinStatus::Fetching | PinStatus::NoChange => String::new(),
    }
}

fn plain_line(name: &str, st: &PinStatus) -> Option<String> {
    match *st {
        PinStatus::Updated {
            ref old,
            ref new,
            comparison,
        } => {
            Some(format!(
                "{name}: {old} -> {new}{}",
                comparison_suffix(comparison)
            ))
        },
        PinStatus::NoChange => Some(format!("{name}: unchanged")),
        PinStatus::Drift {
            ref rev,
            accepted: false,
        } => {
            Some(format!(
                "{name}: DRIFT: rev {rev} unchanged but content differs (lock kept)"
            ))
        },
        PinStatus::Drift {
            ref rev,
            accepted: true,
        } => {
            Some(format!(
                "{name}: DRIFT: rev {rev} content changed, relocked (--accept)"
            ))
        },
        PinStatus::FixedDrift {
            ref old,
            ref new,
            accepted: false,
        } => {
            Some(format!(
                "{name}: DRIFT: fixed pin sha256 changed {old} -> {new} (lock kept; --accept to \
                 relock)"
            ))
        },
        PinStatus::FixedDrift {
            ref old,
            ref new,
            accepted: true,
        } => {
            Some(format!(
                "{name}: DRIFT: fixed pin sha256 changed {old} -> {new}, relocked (--accept)"
            ))
        },
        PinStatus::Skipped(ref note) => Some(format!("{name}: {note}")),
        PinStatus::Failed(ref msg) => Some(format!("{name}: FAILED: {msg}")),
        PinStatus::Pending | PinStatus::Fetching => None,
    }
}

const fn comparison_suffix(comparison: BranchComparison) -> &'static str {
    match comparison.status {
        Some(CompareStatus::Ahead) => " (ahead)",
        Some(CompareStatus::Behind) => " (behind)",
        Some(CompareStatus::Diverged) => " (diverged)",
        None if comparison.expected => " (unverified)",
        Some(CompareStatus::Identical) | None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_comparison_suffix_marks_unverified_results() {
        assert_eq!(
            comparison_suffix(BranchComparison::verified(CompareStatus::Ahead)),
            " (ahead)"
        );
        assert_eq!(
            comparison_suffix(BranchComparison::unavailable()),
            " (unverified)"
        );
        assert_eq!(comparison_suffix(BranchComparison::none()), "");
    }
}
