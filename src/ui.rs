// SPDX-License-Identifier: EUPL-1.2

use std::{
    fmt,
    fmt::Write as _,
    io::{
        self,
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
    Height,
    Width,
    terminal_size,
};
use unicode_width::UnicodeWidthChar as _;

use crate::{
    fetch::{
        BranchComparison,
        CommitLog,
        CompareStatus,
    },
    render::printable,
    style::{
        Sgr,
        Style,
    },
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

struct Row {
    name:   String,
    header: Option<GroupHeader>,
}

struct GroupHeader {
    label: String,
    first: bool,
}

impl GroupHeader {
    fn tty(&self) -> String {
        format!("{}{}", self.gap(), Sgr::Bold.wrap(&self.label))
    }

    fn plain(&self) -> String {
        format!("{}# {}", self.gap(), self.label)
    }

    const fn gap(&self) -> &'static str {
        if self.first { "" } else { "\n" }
    }
}

pub struct Display {
    states: Arc<Mutex<Vec<PinStatus>>>,
    rows:   Arc<[Row]>,
    stop:   Arc<AtomicBool>,
    drawn:  Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
    tty:    bool,
}

impl Display {
    /// takes `(name, group)` pairs already clustered by group, and skips
    /// headers entirely when no pin has a group
    pub fn new(pins: Vec<(String, Option<String>)>) -> Self {
        let tty = Style::stdout().is_tty();
        let states = Arc::new(Mutex::new(vec![PinStatus::Pending; pins.len()]));
        let grouped = pins.iter().any(|&(_, ref group)| group.is_some());
        let mut previous = None;
        let rows = pins
            .into_iter()
            .enumerate()
            .map(|(index, (name, group))| {
                let starts_group = index == 0 || previous.as_ref() != Some(&group);
                let header = (grouped && starts_group).then(|| {
                    GroupHeader {
                        label: group.clone().unwrap_or_else(|| "ungrouped".to_owned()),
                        first: index == 0,
                    }
                });
                previous = Some(group);
                Row { name, header }
            })
            .collect::<Arc<[Row]>>();
        let stop = Arc::new(AtomicBool::new(false));
        let drawn = Arc::new(AtomicUsize::new(0));

        let handle = tty.then(|| {
            let states_for_draw = Arc::clone(&states);
            let rows_for_draw = Arc::clone(&rows);
            let stop_for_draw = Arc::clone(&stop);
            let drawn_for_draw = Arc::clone(&drawn);
            thread::spawn(move || {
                let mut drawn_rows = 0_usize;
                let mut frame = 0;
                while !stop_for_draw.load(Ordering::Relaxed) {
                    drawn_rows = FrameRenderer::new(
                        &rows_for_draw,
                        &states_for_draw.lock().unwrap(),
                        frame,
                        drawn_rows,
                    )
                    .draw();
                    drawn_for_draw.store(drawn_rows, Ordering::Relaxed);
                    frame = frame.wrapping_add(1);
                    thread::sleep(Duration::from_millis(67));
                }
                drawn_rows = FrameRenderer::new(
                    &rows_for_draw,
                    &states_for_draw.lock().unwrap(),
                    frame,
                    drawn_rows,
                )
                .draw();
                drawn_for_draw.store(drawn_rows, Ordering::Relaxed);
            })
        });
        Self {
            states,
            rows,
            stop,
            drawn,
            handle,
            tty,
        }
    }

    pub fn set(&self, i: usize, status: PinStatus) {
        self.states.lock().unwrap()[i] = status;
    }

    pub fn finish(self) {
        self.finish_verbose(&[]);
    }

    pub fn finish_verbose(mut self, logs: &[Option<CommitLog>]) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let states = self.states.lock().unwrap();
        let mut out = io::stdout().lock();
        if self.tty {
            let mut rendered = Vec::new();
            for (index, (row, status)) in self.rows.iter().zip(states.iter()).enumerate() {
                if let Some(ref header) = row.header {
                    let _ = writeln!(rendered, "{}", header.tty());
                }
                let line = StatusLine::new(&row.name, status);
                let _ = writeln!(rendered, "{}", line.tty());
                if line.is_updated()
                    && let Some(log) = logs.get(index).and_then(Option::as_ref)
                {
                    let indent = " ".repeat(4 + row.name.len() + 2);
                    CommitLogLines::new(&indent, log).write_to(&mut rendered);
                }
            }

            // replace live spinner rows line by line, since tmux's scroll-on-clear
            // pushes the screen into scrollback when `\x1b[J` runs from the top-left
            let list = String::from_utf8_lossy(&rendered).replace('\n', "\n\x1b[2K");
            let drawn = self.drawn.load(Ordering::Relaxed);
            let _ = write!(out, "\x1b[{drawn}A\x1b[2K{list}\x1b[J");
        } else {
            for (index, (row, status)) in self.rows.iter().zip(states.iter()).enumerate() {
                if let Some(ref header) = row.header {
                    let _ = writeln!(out, "{}", header.plain());
                }
                let line = StatusLine::new(&row.name, status);
                if let Some(text) = line.plain() {
                    let _ = writeln!(out, "{text}");
                }
                if line.is_updated()
                    && let Some(log) = logs.get(index).and_then(Option::as_ref)
                {
                    let indent = " ".repeat(row.name.len() + 2);
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
        let _ = writeln!(
            out,
            "{}{}",
            self.indent,
            CommitLogSummary::new(self.log).text()
        );

        for &(ref hash, ref subject) in &self.log.fresh {
            // upstream controls subjects, so strip escapes a terminal would act on
            let _ = writeln!(
                out,
                "{}{}    {}",
                self.indent,
                CommitHash::new(hash).short(),
                printable(subject)
            );
        }

        let elided = self.log.total.saturating_sub(self.log.fresh.len());
        if elided > 0 {
            let _ = writeln!(out, "{}... {} elided", self.indent, elided);
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

struct CommitLogSummary<'a> {
    log: &'a CommitLog,
}

impl<'a> CommitLogSummary<'a> {
    const fn new(log: &'a CommitLog) -> Self {
        Self { log }
    }

    fn text(&self) -> String {
        match (self.log.ahead, self.log.behind) {
            (ahead, 0) => format!("{ahead} ahead"),
            (0, behind) => format!("{behind} behind"),
            (ahead, behind) => {
                format!("{ahead} ahead, {behind} behind")
            },
        }
    }
}

struct FrameRenderer<'a> {
    rows:       &'a [Row],
    states:     &'a [PinStatus],
    frame:      usize,
    drawn_rows: usize,
}

impl<'a> FrameRenderer<'a> {
    const fn new(
        rows: &'a [Row],
        states: &'a [PinStatus],
        frame: usize,
        drawn_rows: usize,
    ) -> Self {
        Self {
            rows,
            states,
            frame,
            drawn_rows,
        }
    }

    fn draw(&self) -> usize {
        let mut out = String::new();
        let (terminal_width, terminal_height) = Self::terminal_dimensions();

        if self.drawn_rows > 0 {
            let _ = write!(out, "\x1b[{}A", self.drawn_rows);
        }

        let lines = self
            .rows
            .iter()
            .zip(self.states)
            .flat_map(|(row, status)| {
                let header = row.header.as_ref().map(GroupHeader::tty);
                let line = StatusLine::new(&row.name, status).tty_with_frame(self.frame);
                header
                    .map(|text| (text, false))
                    .into_iter()
                    .chain([(line, true)])
            })
            .collect::<Vec<_>>();
        let segments = lines
            .iter()
            .flat_map(|&(ref text, pin)| Self::terminal_segments(text).map(move |seg| (seg, pin)))
            .collect::<Vec<_>>();

        // cursor-up clamps at the top of the screen, so a frame taller than the
        // terminal scrolls its first line into scrollback on every redraw
        let budget = terminal_height.saturating_sub(2);
        let mut rows = 0_usize;
        for (index, &(segment, _)) in segments.iter().enumerate() {
            let height = Self::visual_rows(segment, terminal_width);
            if rows + height > budget {
                let hidden = segments[index..].iter().filter(|&&(_, pin)| pin).count();
                out.push_str("\x1b[2K");
                let _ = writeln!(out, "… {hidden} more");
                rows += 1;
                break;
            }
            out.push_str("\x1b[2K");
            let _ = writeln!(out, "{segment}");
            rows += height;
        }

        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
        rows
    }

    fn terminal_dimensions() -> (usize, usize) {
        terminal_size().map_or((80, 24), |(Width(width), Height(height))| {
            (usize::from(width).max(1), usize::from(height))
        })
    }

    fn terminal_segments(line: &str) -> impl Iterator<Item = &str> {
        line.split('\n')
            .map(|segment| segment.strip_suffix('\r').unwrap_or(segment))
    }

    fn visual_rows(line: &str, terminal_width: usize) -> usize {
        Self::visible_width(line).saturating_sub(1) / terminal_width + 1
    }

    fn visible_width(line: &str) -> usize {
        let mut width = 0_usize;
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                Self::skip_ansi_escape(&mut chars);
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
    color: Sgr,
    ch:    char,
}

struct FramedStatus<'a> {
    status: &'a PinStatus,
    frame:  usize,
}

impl From<&PinStatus> for StatusGlyph {
    fn from(status: &PinStatus) -> Self {
        let (color, ch) = match *status {
            PinStatus::Fetching { frame } => (Sgr::Blue, FRAMES[frame % FRAMES.len()]),
            PinStatus::NoChange => (Sgr::Green, '\u{2713}'),
            PinStatus::Updated { .. } => (Sgr::Yellow, '*'),
            PinStatus::Drift { accepted: true, .. }
            | PinStatus::FixedDrift { accepted: true, .. } => (Sgr::Yellow, '~'),
            PinStatus::Drift {
                accepted: false, ..
            }
            | PinStatus::FixedDrift {
                accepted: false, ..
            } => (Sgr::Red, '!'),
            PinStatus::Pending | PinStatus::Skipped(_) => (Sgr::Dim, '\u{b7}'),
            PinStatus::Failed(_) => (Sgr::Red, '\u{2717}'),
        };
        Self { color, ch }
    }
}

impl From<FramedStatus<'_>> for StatusGlyph {
    fn from(value: FramedStatus<'_>) -> Self {
        match *value.status {
            PinStatus::Fetching { .. } => {
                Self {
                    color: Sgr::Blue,
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
        self.color.wrap(&self.ch.to_string())
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
