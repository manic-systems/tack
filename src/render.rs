// SPDX-License-Identifier: EUPL-1.2

use std::fmt;

use crate::{
    history,
    report::{
        CollapsedFollow,
        DedupReport,
        Mark,
    },
    scan_diagnostic::ScanDiagnostic,
};

const MAX_SOURCES: usize = 5;

pub fn short(rev: &str) -> String {
    fn trim(seg: &str) -> &str {
        let str = seg.split('?').next().unwrap_or(seg);
        str.split('#').next().unwrap_or(str)
    }
    if rev.contains("://") {
        let segs = rev
            .split_once("://")
            .map_or("", |x| x.1)
            .split('/')
            .filter(|seg| !seg.is_empty())
            .collect::<Vec<&str>>();

        let pick = match segs.len() {
            0 => None,
            1 => Some(trim(segs[0])),
            n => Some(trim(segs[n - 2])),
        };

        if let Some(seg) = pick {
            return seg.chars().take(16).collect();
        }
    }
    if let Some(b64) = rev.strip_prefix("sha256-") {
        return format!("sha256-{}", b64.chars().take(12).collect::<String>());
    }
    rev.chars().take(7).collect()
}

pub fn source_label(path: &[String]) -> String {
    if path.is_empty() {
        "top".into()
    } else {
        path.join(" > ")
    }
}

pub fn scan_diagnostic(diagnostic: &ScanDiagnostic) -> String {
    format!("scan {}: {}", source_label(diagnostic.path()), diagnostic)
}

/// radius-1 window around the cursor
pub fn render_window(view: &history::View) {
    let lo = view.cursor.saturating_sub(1);
    let hi = (view.cursor + 1).min(view.rows.len().saturating_sub(1));
    render(view, lo, hi);
}

/// rows `lo..=hi` newest-first, `>` marks the cursor
pub fn render(view: &history::View, lo: usize, hi: usize) {
    let now = history::now();
    let times = (lo..=hi)
        .map(|idx| history::rel_time(now, view.rows[idx].ts))
        .collect::<Vec<String>>();
    let width = times.iter().map(String::len).max().unwrap_or(0);
    for idx in (lo..=hi).rev() {
        let marker = if idx == view.cursor { '>' } else { ' ' };
        let when = &times[idx - lo];
        println!("{marker} {when:width$}  {}", view.rows[idx].label);
    }
}

pub fn print_report(report: &DedupReport) {
    if report.groups.is_empty() {
        println!("no duplicate inputs found");
        return;
    }

    for group in &report.groups {
        println!("\n{}  x{}", group.id, group.count);

        let rw = group
            .revs
            .iter()
            .map(|rev| short(&rev.rev).len())
            .max()
            .unwrap_or(0);
        let nw = group
            .revs
            .iter()
            .flat_map(|rev| rev.names.iter().map(|name| name.name.len()))
            .max()
            .unwrap_or(0);
        let marks = group
            .revs
            .iter()
            .map(|rev| RenderedMark::from(rev.mark))
            .collect::<Vec<_>>();
        let mw = marks.iter().map(|mark| mark.width).max().unwrap_or(1);

        for (rev, mark) in group.revs.iter().zip(marks) {
            let rendered_rev = short(&rev.rev);
            let mark_on = format!("{mark}{}", " ".repeat(mw - mark.width));
            let blank = " ".repeat(mw);
            for name in &rev.names {
                let shown = name.sources.len().min(MAX_SOURCES);
                for (idx, source_path) in name.sources.iter().take(shown).enumerate() {
                    let rev_cell = if idx == 0 { rendered_rev.as_str() } else { "" };
                    let mark_cell = if idx == 0 {
                        mark_on.as_str()
                    } else {
                        blank.as_str()
                    };
                    let name_cell = if idx == 0 { name.name.as_str() } else { "" };
                    let rendered_source = source_label(source_path);
                    println!("  {rev_cell:rw$} {mark_cell} {name_cell:nw$}  {rendered_source}");
                }
                if name.sources.len() > shown {
                    let extra = name.sources.len() - shown;
                    println!(
                        "  {empty:rw$} {blank} {empty:nw$}  ...{extra} more",
                        empty = ""
                    );
                }
            }
        }
    }

    if report.follows.is_empty() {
        return;
    }
    let pin_lines = report.follows.pin.collapsed();
    let auto_lines = report.follows.auto.collapsed();
    let kw = pin_lines
        .iter()
        .chain(auto_lines.iter())
        .map(|line| follow_key(line).len())
        .max()
        .unwrap_or(0);
    println!("\nshare via [all_follow] in pins.toml:");
    for line in &pin_lines {
        let key = follow_key(line);
        let rhs = follow_rhs(line);
        println!("  {key:kw$} = {rhs}");
    }
    if !auto_lines.is_empty() {
        if !pin_lines.is_empty() {
            println!();
        }
        println!("  # auto-dedup (no top-level pin needed):");
        for line in &auto_lines {
            let key = follow_key(line);
            let rhs = follow_rhs(line);
            println!("  {key:kw$} = {rhs}");
        }
    }
}

struct RenderedMark {
    text:  String,
    width: usize,
}

impl From<Mark> for RenderedMark {
    fn from(mark: Mark) -> Self {
        const APPROX: &str = "~";
        let paint = |code: i32, body: &str| format!("\x1b[{code}m{body}\x1b[0m");
        let (text, width) = match mark {
            Mark::Base => (paint(36_i32, "="), 1),
            Mark::Ahead => (paint(32_i32, "\u{2191}"), 1),
            Mark::Behind => (paint(33_i32, "\u{2193}"), 1),
            Mark::Diverged => {
                (
                    format!("{}{}", paint(32_i32, "\u{2191}"), paint(33_i32, "\u{2193}")),
                    2,
                )
            },
            Mark::DatedNewer => {
                (
                    format!("{}{}", paint(32_i32, "\u{2191}"), paint(36_i32, APPROX)),
                    2,
                )
            },
            Mark::DatedOlder => {
                (
                    format!("{}{}", paint(33_i32, "\u{2193}"), paint(36_i32, APPROX)),
                    2,
                )
            },
            Mark::DatedEqual => (paint(36_i32, APPROX), 1),
            Mark::Unknown => (" ".to_owned(), 1),
        };
        Self { text, width }
    }
}

impl fmt::Display for RenderedMark {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

fn follow_key(follow: &CollapsedFollow) -> &str {
    match *follow {
        CollapsedFollow::Single { ref alias, .. } => alias,
        CollapsedFollow::Group { ref target, .. } => target,
    }
}

fn follow_rhs(follow: &CollapsedFollow) -> String {
    match *follow {
        CollapsedFollow::Single { ref target, .. } => format!("\"{target}\""),
        CollapsedFollow::Group { ref aliases, .. } => {
            let body = aliases
                .iter()
                .map(|alias| format!("\"{alias}\""))
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{body}]")
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RenderedMark,
        follow_key,
        follow_rhs,
        scan_diagnostic,
    };
    use crate::{
        report::{
            CollapsedFollow,
            FollowMap,
            Mark,
        },
        scan_diagnostic::{
            ScanDiagnostic,
            ScanFile,
        },
    };

    fn map(pairs: &[(&str, &str)]) -> FollowMap {
        let mut follow = FollowMap::default();
        for &(alias, target) in pairs {
            follow.insert(alias.to_owned(), target.to_owned());
        }
        follow
    }

    fn rendered(lines: &[CollapsedFollow]) -> Vec<(String, String)> {
        lines
            .iter()
            .map(|follow| (follow_key(follow).to_owned(), follow_rhs(follow)))
            .collect()
    }

    #[test]
    fn collapse_single_alias_uses_string_form() {
        let lines = map(&[("nixpkgs", "nixpkgs")]).collapsed();
        assert_eq!(rendered(&lines), vec![(
            "nixpkgs".into(),
            "\"nixpkgs\"".into()
        )]);
    }

    #[test]
    fn collapse_multi_alias_uses_array_form_excluding_key() {
        let lines = map(&[("git-hooks", "git-hooks"), ("git-hooks-nix", "git-hooks")]).collapsed();
        assert_eq!(rendered(&lines), vec![(
            "git-hooks".into(),
            "[\"git-hooks-nix\"]".into()
        )]);
    }

    #[test]
    fn collapse_multi_alias_when_target_is_not_an_alias() {
        let lines = map(&[("xwl-stable", "xwl"), ("xwl-unstable", "xwl")]).collapsed();
        assert_eq!(rendered(&lines), vec![(
            "xwl".into(),
            "[\"xwl-stable\", \"xwl-unstable\"]".into()
        )]);
    }

    #[test]
    fn mark_glyphs_keep_visible_widths() {
        let ahead = RenderedMark::from(Mark::Ahead);
        assert_eq!(ahead.width, 1);
        assert!(ahead.text.contains('\u{2191}'));
        assert!(!ahead.text.contains('~'));

        let diverged = RenderedMark::from(Mark::Diverged);
        assert_eq!(diverged.width, 2);
        assert!(diverged.text.contains('\u{2191}'));
        assert!(diverged.text.contains('\u{2193}'));
        assert!(!diverged.text.contains('~'));

        let dated = RenderedMark::from(Mark::DatedNewer);
        assert_eq!(dated.width, 2);
        assert!(dated.text.contains('\u{2191}'));
        assert!(dated.text.contains('~'));
    }

    #[test]
    fn scan_diagnostic_includes_source_file_and_kind() {
        let path = vec!["root".to_owned(), "dep".to_owned()];
        let diagnostic = ScanDiagnostic::parse(&path, ScanFile::FlakeLock, "expected value");

        assert_eq!(
            scan_diagnostic(&diagnostic),
            "scan root > dep: flake.lock parse failed: expected value"
        );
    }
}
