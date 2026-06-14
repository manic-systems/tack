// SPDX-License-Identifier: EUPL-1.2

use std::{
    fmt,
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
            AtomicUsize,
            Ordering,
        },
    },
    thread::{
        self,
        JoinHandle,
    },
    time::Duration,
};

use terminal_size::{
    Width,
    terminal_size,
};
use unicode_width::UnicodeWidthChar as _;

use crate::fetch::{
    BranchComparison,
    CompareStatus,
    github::CommitLog,
};

#[derive(Clone)]
pub enum PinStatus {
    Pending,
    Fetching {
        frame: usize,
    },
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
    FixedDrift {
        old:      String,
        new:      String,
        accepted: bool,
    },
    Skipped(String),
    Failed(String),
}

const FRAMES: [char; 4] = ['/', '-', '\\', '|'];

pub struct Display {
    states: Arc<Mutex<Vec<PinStatus>>>,
    names:  Arc<[String]>,
    stop:   Arc<AtomicBool>,
    rows:   Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
    tty:    bool,
}

impl Display {
    pub fn new(initial_names: Vec<String>) -> Self {
        let tty = io::stdout().is_terminal();
        let states = Arc::new(Mutex::new(vec![PinStatus::Pending; initial_names.len()]));
        let names = initial_names.into();
        let stop = Arc::new(AtomicBool::new(false));
        let rows = Arc::new(AtomicUsize::new(0));

        let handle = tty.then(|| {
            let states_for_draw = Arc::clone(&states);
            let names_for_draw = Arc::clone(&names);
            let stop_for_draw = Arc::clone(&stop);
            let rows_for_draw = Arc::clone(&rows);
            thread::spawn(move || {
                let mut drawn_rows = 0_usize;
                let mut frame = 0;
                while !stop_for_draw.load(Ordering::Relaxed) {
                    drawn_rows = FrameRenderer::new(
                        &names_for_draw,
                        &states_for_draw.lock().unwrap(),
                        frame,
                        drawn_rows,
                    )
                    .draw();
                    rows_for_draw.store(drawn_rows, Ordering::Relaxed);
                    frame = frame.wrapping_add(1);
                    thread::sleep(Duration::from_millis(67));
                }
                drawn_rows = FrameRenderer::new(
                    &names_for_draw,
                    &states_for_draw.lock().unwrap(),
                    frame,
                    drawn_rows,
                )
                .draw();
                rows_for_draw.store(drawn_rows, Ordering::Relaxed);
            })
        });
        Self {
            states,
            names,
            stop,
            rows,
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
                if let Some(line) = StatusLine::new(name, st).plain() {
                    println!("{line}");
                }
            }
        }
    }

    pub fn finish_verbose(mut self, logs: &[Option<CommitLog>]) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let states = self.states.lock().unwrap();
        let mut out = io::stdout().lock();
        if self.tty {
            // replace live spinner rows
            let _ = write!(out, "\x1b[{}A\x1b[J", self.rows.load(Ordering::Relaxed));
            for ((name, status), entry) in self.names.iter().zip(states.iter()).zip(logs.iter()) {
                let line = StatusLine::new(name, status);
                let _ = writeln!(out, "{}", line.tty());
                if line.is_updated()
                    && let Some(log) = entry.as_ref()
                {
                    let indent = " ".repeat(4 + name.len() + 2);
                    CommitLogLines::new(&indent, log).write_to(&mut out);
                }
            }
        } else {
            for ((name, status), entry) in self.names.iter().zip(states.iter()).zip(logs.iter()) {
                let line = StatusLine::new(name, status);
                if let Some(text) = line.plain() {
                    let _ = writeln!(out, "{text}");
                }
                if line.is_updated()
                    && let Some(log) = entry.as_ref()
                {
                    let indent = " ".repeat(name.len() + 2);
                    CommitLogLines::new(&indent, log).write_to(&mut out);
                }
            }
        }
        let _ = out.flush();
    }
}

struct CommitHash<'a> {
    value: &'a str,
}

impl<'a> CommitHash<'a> {
    const fn new(value: &'a str) -> Self {
        Self { value }
    }

    fn short(&self) -> &'a str {
        self.value.get(..7).unwrap_or(self.value)
    }
}

struct CommitLogLines<'a> {
    indent: &'a str,
    log:    &'a CommitLog,
}

impl<'a> CommitLogLines<'a> {
    const fn new(indent: &'a str, log: &'a CommitLog) -> Self {
        Self { indent, log }
    }

    fn write_to(&self, out: &mut dyn io::Write) {
        let mut fresh = self.log.fresh.iter();

        if let Some(&(ref hash, ref subject)) = fresh.next() {
            let _ = writeln!(
                out,
                "{}{}    {subject}",
                self.indent,
                CommitHash::new(hash).short()
            );
        }

        for &(ref hash, ref subject) in fresh {
            let _ = writeln!(
                out,
                "{}{}    {subject}",
                self.indent,
                CommitHash::new(hash).short()
            );
        }

        if self.log.more {
            let _ = writeln!(out, "{}...", self.indent);
        }

        if let Some((ref hash, ref subject)) = self.log.base {
            let _ = writeln!(
                out,
                "{}{}    {subject}",
                self.indent,
                CommitHash::new(hash).short()
            );
        }
    }
}

struct FrameRenderer<'a> {
    names:      &'a [String],
    states:     &'a [PinStatus],
    frame:      usize,
    drawn_rows: usize,
}

impl<'a> FrameRenderer<'a> {
    const fn new(
        names: &'a [String],
        states: &'a [PinStatus],
        frame: usize,
        drawn_rows: usize,
    ) -> Self {
        Self {
            names,
            states,
            frame,
            drawn_rows,
        }
    }

    fn draw(&self) -> usize {
        let mut out = String::new();
        let terminal_width = terminal_width();

        if self.drawn_rows > 0 {
            let _ = write!(out, "\x1b[{}A", self.drawn_rows);
        }

        let mut rows = 0_usize;
        for (name, status) in self.names.iter().zip(self.states) {
            let line = StatusLine::new(name, status).tty_with_frame(self.frame);
            for segment in terminal_segments(&line) {
                out.push_str("\x1b[2K");
                let _ = writeln!(out, "{segment}");
                rows += visual_rows(segment, terminal_width);
            }
        }

        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
        rows
    }
}

fn terminal_width() -> usize {
    terminal_size().map_or(80, |(Width(width), _)| usize::from(width).max(1))
}

fn terminal_segments(line: &str) -> impl Iterator<Item = &str> {
    line.split('\n')
        .map(|segment| segment.strip_suffix('\r').unwrap_or(segment))
}

fn visual_rows(line: &str, terminal_width: usize) -> usize {
    visible_width(line).saturating_sub(1) / terminal_width + 1
}

fn visible_width(line: &str) -> usize {
    let mut width = 0_usize;
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            skip_ansi_escape(&mut chars);
        } else {
            width += ch.width().unwrap_or(0);
        }
    }
    width
}

fn skip_ansi_escape(chars: &mut impl Iterator<Item = char>) {
    if chars.next() != Some('[') {
        return;
    }
    for ch in chars {
        if ('@'..='~').contains(&ch) {
            break;
        }
    }
}

struct StatusLine<'a> {
    name:   &'a str,
    status: &'a PinStatus,
}

impl<'a> StatusLine<'a> {
    const fn new(name: &'a str, status: &'a PinStatus) -> Self {
        Self { name, status }
    }

    fn tty(&self) -> String {
        format!(
            "[{}] {}{}",
            StatusGlyph::from(self.status).ansi(),
            self.name,
            self.suffix()
        )
    }

    fn tty_with_frame(&self, frame: usize) -> String {
        format!(
            "[{}] {}{}",
            StatusGlyph::from(FramedStatus {
                status: self.status,
                frame,
            })
            .ansi(),
            self.name,
            self.suffix()
        )
    }

    const fn is_updated(&self) -> bool {
        matches!(*self.status, PinStatus::Updated { .. })
    }

    fn suffix(&self) -> String {
        match *self.status {
            PinStatus::Updated {
                ref old,
                ref new,
                comparison,
            } => {
                format!("  {old} -> {new}{}", ComparisonLabel::new(comparison))
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
                    "  DRIFT: fixed pin sha256 changed {old} -> {new} (lock kept; --accept to \
                     relock)"
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
            PinStatus::Pending | PinStatus::Fetching { .. } | PinStatus::NoChange => String::new(),
        }
    }

    fn plain(&self) -> Option<String> {
        match *self.status {
            PinStatus::Updated {
                ref old,
                ref new,
                comparison,
            } => {
                Some(format!(
                    "{}: {old} -> {new}{}",
                    self.name,
                    ComparisonLabel::new(comparison)
                ))
            },
            PinStatus::NoChange => Some(format!("{}: unchanged", self.name)),
            PinStatus::Drift {
                ref rev,
                accepted: false,
            } => {
                Some(format!(
                    "{}: DRIFT: rev {rev} unchanged but content differs (lock kept)",
                    self.name
                ))
            },
            PinStatus::Drift {
                ref rev,
                accepted: true,
            } => {
                Some(format!(
                    "{}: DRIFT: rev {rev} content changed, relocked (--accept)",
                    self.name
                ))
            },
            PinStatus::FixedDrift {
                ref old,
                ref new,
                accepted: false,
            } => {
                Some(format!(
                    "{}: DRIFT: fixed pin sha256 changed {old} -> {new} (lock kept; --accept to \
                     relock)",
                    self.name
                ))
            },
            PinStatus::FixedDrift {
                ref old,
                ref new,
                accepted: true,
            } => {
                Some(format!(
                    "{}: DRIFT: fixed pin sha256 changed {old} -> {new}, relocked (--accept)",
                    self.name
                ))
            },
            PinStatus::Skipped(ref note) => Some(format!("{}: {note}", self.name)),
            PinStatus::Failed(ref msg) => Some(format!("{}: FAILED: {msg}", self.name)),
            PinStatus::Pending | PinStatus::Fetching { .. } => None,
        }
    }
}

struct StatusGlyph {
    color: i32,
    ch:    char,
}

struct FramedStatus<'a> {
    status: &'a PinStatus,
    frame:  usize,
}

impl From<&PinStatus> for StatusGlyph {
    fn from(status: &PinStatus) -> Self {
        let (color, ch) = match *status {
            PinStatus::Fetching { frame } => (34_i32, FRAMES[frame % FRAMES.len()]),
            PinStatus::NoChange => (32_i32, '\u{2713}'),
            PinStatus::Updated { .. } => (33_i32, '*'),
            PinStatus::Drift { accepted: true, .. }
            | PinStatus::FixedDrift { accepted: true, .. } => (33_i32, '~'),
            PinStatus::Drift {
                accepted: false, ..
            }
            | PinStatus::FixedDrift {
                accepted: false, ..
            } => (31_i32, '!'),
            PinStatus::Pending | PinStatus::Skipped(_) => (2_i32, '\u{b7}'),
            PinStatus::Failed(_) => (31_i32, '\u{2717}'),
        };
        Self { color, ch }
    }
}

impl From<FramedStatus<'_>> for StatusGlyph {
    fn from(value: FramedStatus<'_>) -> Self {
        match *value.status {
            PinStatus::Fetching { .. } => {
                Self {
                    color: 34_i32,
                    ch:    FRAMES[value.frame % FRAMES.len()],
                }
            },
            PinStatus::Pending
            | PinStatus::NoChange
            | PinStatus::Updated { .. }
            | PinStatus::Drift { .. }
            | PinStatus::FixedDrift { .. }
            | PinStatus::Skipped(_)
            | PinStatus::Failed(_) => Self::from(value.status),
        }
    }
}

impl StatusGlyph {
    fn ansi(&self) -> String {
        format!("\x1b[{}m{}\x1b[0m", self.color, self.ch)
    }
}

struct ComparisonLabel {
    comparison: BranchComparison,
}

impl ComparisonLabel {
    const fn new(comparison: BranchComparison) -> Self {
        Self { comparison }
    }
}

impl fmt::Display for ComparisonLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self.comparison.status {
            Some(CompareStatus::Ahead) => " (ahead)",
            Some(CompareStatus::Behind) => " (behind)",
            Some(CompareStatus::Diverged) => " (diverged)",
            None if self.comparison.expected => " (unverified)",
            Some(CompareStatus::Identical) | None => "",
        };
        f.write_str(text)
    }
}
