// SPDX-License-Identifier: EUPL-1.2

use std::collections::{
    BTreeMap,
    BTreeSet,
};

use crate::{
    commands::dedup::{
        DedupReport,
        Mark,
    },
    history,
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

/// A radius-1 window around the new cursor: the redo target, the live state,
/// the undo target.
pub fn render_window(view: &history::View) {
    let lo = view.cursor.saturating_sub(1);
    let hi = (view.cursor + 1).min(view.rows.len().saturating_sub(1));
    render(view, lo, hi);
}

/// Rows `lo..=hi` newest-first, relative times aligned, `>` marking the cursor.
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
            .map(|rev| rev.rev.len())
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
            .map(|rev| mark_glyph(rev.mark))
            .collect::<Vec<_>>();
        let mw = marks.iter().map(|&(_, vis)| vis).max().unwrap_or(1);

        for (rev, (mark, width)) in group.revs.iter().zip(marks) {
            let mark_on = format!("{mark}{}", " ".repeat(mw - width));
            let blank = " ".repeat(mw);
            for name in &rev.names {
                let shown = name.sources.len().min(MAX_SOURCES);
                for (idx, source) in name.sources.iter().take(shown).enumerate() {
                    let rev_cell = if idx == 0 { rev.rev.as_str() } else { "" };
                    let mark_cell = if idx == 0 {
                        mark_on.as_str()
                    } else {
                        blank.as_str()
                    };
                    let name_cell = if idx == 0 { name.name.as_str() } else { "" };
                    println!("  {rev_cell:rw$} {mark_cell} {name_cell:nw$}  {source}");
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

    if report.pin_follow.is_empty() && report.auto_follow.is_empty() {
        return;
    }
    let pin_lines = collapse_follow(&report.pin_follow);
    let auto_lines = collapse_follow(&report.auto_follow);
    let kw = pin_lines
        .iter()
        .chain(auto_lines.iter())
        .map(|&(ref key, _)| key.len())
        .max()
        .unwrap_or(0);
    println!("\nshare via [all_follow] in pins.toml:");
    for &(ref key, ref rhs) in &pin_lines {
        println!("  {key:kw$} = {rhs}");
    }
    if !auto_lines.is_empty() {
        if !pin_lines.is_empty() {
            println!();
        }
        println!("  # auto-dedup (no top-level pin needed):");
        for &(ref key, ref rhs) in &auto_lines {
            println!("  {key:kw$} = {rhs}");
        }
    }
}

fn mark_glyph(mark: Mark) -> (String, usize) {
    const APPROX: &str = "~";
    let paint = |code: i32, body: &str| format!("\x1b[{code}m{body}\x1b[0m");
    match mark {
        Mark::Base => (paint(36, "="), 1),
        Mark::Ahead => (paint(32, "\u{2191}"), 1),
        Mark::Behind => (paint(33, "\u{2193}"), 1),
        Mark::Diverged => {
            (
                format!("{}{}", paint(32, "\u{2191}"), paint(33, "\u{2193}")),
                2,
            )
        },
        Mark::DatedNewer => (format!("{}{}", paint(32, "\u{2191}"), paint(36, APPROX)), 2),
        Mark::DatedOlder => (format!("{}{}", paint(33, "\u{2193}"), paint(36, APPROX)), 2),
        Mark::DatedEqual => (paint(36, APPROX), 1),
        Mark::Unknown => (" ".to_owned(), 1),
    }
}

/// Invert alias -> target into target -> aliases and emit one line per target.
/// Single-alias groups use string form and multi-alias groups use array form.
fn collapse_follow(follow: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut by_target = BTreeMap::<&str, BTreeSet<&str>>::new();
    for (alias, target) in follow {
        by_target
            .entry(target.as_str())
            .or_default()
            .insert(alias.as_str());
    }
    let mut lines = Vec::<(String, String)>::new();
    for (target, aliases) in &by_target {
        if aliases.len() == 1 {
            let alias = aliases.iter().next().copied().unwrap_or("");
            lines.push((alias.to_owned(), format!("\"{target}\"")));
        } else {
            let body = aliases
                .iter()
                .filter(|alias| **alias != *target)
                .map(|alias| format!("\"{alias}\""))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(((*target).to_owned(), format!("[{body}]")));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        collapse_follow,
        mark_glyph,
    };
    use crate::commands::dedup::Mark;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|&(alias, target)| (alias.to_owned(), target.to_owned()))
            .collect()
    }

    #[test]
    fn collapse_single_alias_uses_string_form() {
        let lines = collapse_follow(&map(&[("nixpkgs", "nixpkgs")]));
        assert_eq!(lines, vec![("nixpkgs".into(), "\"nixpkgs\"".into())]);
    }

    #[test]
    fn collapse_multi_alias_uses_array_form_excluding_key() {
        let lines = collapse_follow(&map(&[
            ("git-hooks", "git-hooks"),
            ("git-hooks-nix", "git-hooks"),
        ]));
        assert_eq!(lines, vec![(
            "git-hooks".into(),
            "[\"git-hooks-nix\"]".into()
        )]);
    }

    #[test]
    fn collapse_multi_alias_when_target_is_not_an_alias() {
        let lines = collapse_follow(&map(&[("xwl-stable", "xwl"), ("xwl-unstable", "xwl")]));
        assert_eq!(lines, vec![(
            "xwl".into(),
            "[\"xwl-stable\", \"xwl-unstable\"]".into()
        )]);
    }

    #[test]
    fn mark_glyphs_keep_visible_widths() {
        let (ahead, ahead_width) = mark_glyph(Mark::Ahead);
        assert_eq!(ahead_width, 1);
        assert!(ahead.contains('\u{2191}'));
        assert!(!ahead.contains('~'));

        let (diverged, diverged_width) = mark_glyph(Mark::Diverged);
        assert_eq!(diverged_width, 2);
        assert!(diverged.contains('\u{2191}'));
        assert!(diverged.contains('\u{2193}'));
        assert!(!diverged.contains('~'));

        let (dated, dated_width) = mark_glyph(Mark::DatedNewer);
        assert_eq!(dated_width, 2);
        assert!(dated.contains('\u{2191}'));
        assert!(dated.contains('~'));
    }
}
