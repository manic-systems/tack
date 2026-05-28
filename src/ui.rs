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

#[derive(Clone)]
pub enum PinStatus {
    Pending,
    Fetching,
    NoChange,
    Updated { old: String, new: String },
    Drift { rev: String, accepted: bool },
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
        PinStatus::Pending => (2_i32, '\u{b7}'),
        PinStatus::Fetching => (34_i32, FRAMES[frame % FRAMES.len()]),
        PinStatus::NoChange => (33_i32, '-'),
        PinStatus::Updated { .. } => (32_i32, '\u{2713}'),
        PinStatus::Drift { accepted: true, .. } => (33_i32, '~'),
        PinStatus::Drift {
            accepted: false, ..
        } => (31_i32, '!'),
        PinStatus::Failed(_) => (31_i32, '\u{2717}'),
    };
    format!("\x1b[{color}m{ch}\x1b[0m")
}

fn suffix(st: &PinStatus) -> String {
    match *st {
        PinStatus::Updated { ref old, ref new } => format!("  {old} -> {new}"),
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
        PinStatus::Failed(ref msg) => format!("  {msg}"),
        PinStatus::Pending | PinStatus::Fetching | PinStatus::NoChange => String::new(),
    }
}

fn plain_line(name: &str, st: &PinStatus) -> Option<String> {
    match *st {
        PinStatus::Updated { ref old, ref new } => Some(format!("{name}: {old} -> {new}")),
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
        PinStatus::Failed(ref msg) => Some(format!("{name}: FAILED: {msg}")),
        PinStatus::Pending | PinStatus::Fetching => None,
    }
}
