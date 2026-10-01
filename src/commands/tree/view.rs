// SPDX-License-Identifier: EUPL-1.2

use std::collections::{
    BTreeMap,
    BTreeSet,
};

use crate::{
    render,
    report::{
        LockedSource,
        PinLock,
        PinTree,
        TreeInput,
        TreeReport,
        TreeTarget,
    },
    style::{
        Sgr,
        Style,
    },
};

const BRANCH: &str = "\u{251c}\u{2500} ";
const LAST_BRANCH: &str = "\u{2514}\u{2500} ";
const TRUNK: &str = "\u{2502}  ";
const ARROW: &str = "\u{2192}";
const GLYPH_WIDTH: usize = 3;

pub fn print(report: &TreeReport) {
    TreeView::new(report).print(&report.pins);
    for warning in &report.warnings {
        eprintln!("tack: {}", render::printable(warning));
    }
}

struct TreeView<'a> {
    style:   Style,
    copies:  Copies<'a>,
    grouped: bool,
    indent:  &'static str,
    /// where every date starts, so dates line up at any depth
    column:  usize,
}

impl<'a> TreeView<'a> {
    fn new(report: &'a TreeReport) -> Self {
        let grouped = report.pins.iter().any(|pin| pin.group.is_some());
        let indent = if grouped { "  " } else { "" };
        let column = report
            .pins
            .iter()
            .map(|pin| {
                let nested = name_column(&pin.inputs, indent.len() + 2 + GLYPH_WIDTH);
                nested.max(indent.len() + pin.name.chars().count())
            })
            .max()
            .unwrap_or(0);
        Self {
            style: Style::stdout(),
            copies: Copies::of(&report.pins),
            grouped,
            indent,
            column,
        }
    }

    fn print(&self, pins: &[PinTree]) {
        let mut previous = None;
        for pin in pins {
            let group = pin.group.as_deref();
            if self.grouped && previous != Some(group) {
                if previous.is_some() {
                    println!();
                }
                let label = group.unwrap_or("ungrouped");
                if self.style.is_tty() {
                    println!("{}", Sgr::Bold.wrap(label));
                } else {
                    println!("# {label}");
                }
            }
            previous = Some(group);
            self.pin(pin);
        }
    }

    fn pin(&self, pin: &PinTree) {
        let indent = self.indent;
        let name_width = self.column - indent.len();
        let name = self
            .style
            .paint(Sgr::Bold, &format!("{:<name_width$}", pin.name));
        let locked = match pin.lock {
            PinLock::Locked(ref locked) => locked,
            PinLock::Missing => {
                println!("{indent}{name}  not locked, run `tack update {}`", pin.name);
                return;
            },
            PinLock::Unknown(ref kind) => {
                println!(
                    "{indent}{name}  locked as '{}', which this tack cannot read",
                    render::printable(kind)
                );
                return;
            },
        };
        let line = format!("{indent}{name}  {}", self.source(locked));
        self.node(&line, &pin.inputs, &format!("{indent}  "), &pin.name);
    }

    /// tack follows fold onto the node's own line, since they are the deduped,
    /// expected case and would otherwise bury the inputs that bring their own
    /// copy
    fn node(&self, line: &str, inputs: &[TreeInput], prefix: &str, pin: &str) {
        let style = self.style;
        match followed_pins(inputs) {
            Some(follows) => println!("{line}  {}", style.paint(Sgr::Dim, &follows)),
            None => println!("{line}"),
        }

        let branches = inputs
            .iter()
            .filter(|input| !matches!(input.target, TreeTarget::FollowsPin(_)))
            .collect::<Vec<_>>();
        for (index, input) in branches.iter().enumerate() {
            let last = index + 1 == branches.len();
            let glyph = if last { LAST_BRANCH } else { BRANCH };
            let name = &render::printable(&input.name);
            match input.target {
                TreeTarget::Locked {
                    ref source,
                    inputs: ref children,
                } => {
                    let child_line =
                        self.child_line(&format!("{prefix}{glyph}"), name, source, pin);
                    let nested = if last { "   " } else { TRUNK };
                    self.node(&child_line, children, &format!("{prefix}{nested}"), pin);
                },
                TreeTarget::Repeated(ref source) => {
                    let child_line =
                        self.child_line(&format!("{prefix}{glyph}"), name, source, pin);
                    println!(
                        "{child_line}  {}",
                        style.paint(Sgr::Dim, "(inputs listed above)")
                    );
                },
                TreeTarget::Unknown(ref kind) => {
                    let text = format!(
                        "{name}  locked as '{}', which this tack cannot read",
                        render::printable(kind)
                    );
                    println!("{prefix}{glyph}{}", style.paint(Sgr::Dim, &text));
                },
                TreeTarget::FollowsInput(ref path) => {
                    let steps = path
                        .iter()
                        .map(|step| render::printable(step))
                        .collect::<Vec<_>>();
                    let text = format!("{name} follows input '{}'", steps.join("/"));
                    println!("{prefix}{glyph}{}", style.paint(Sgr::Dim, &text));
                },
                TreeTarget::FollowsPin(_) => {},
            }
        }
    }

    fn child_line(&self, lead: &str, name: &str, source: &LockedSource, pin: &str) -> String {
        let style = self.style;
        let others = self.copies.elsewhere(source, pin);
        let also = if others.is_empty() {
            String::new()
        } else {
            style.paint(Sgr::Dim, &format!("  (also in {})", others.join(", ")))
        };
        let name_width = self.column.saturating_sub(lead.chars().count());
        format!(
            "{lead}{}  {}{also}",
            style.paint(Sgr::Yellow, &format!("{name:<name_width$}")),
            self.source(source)
        )
    }

    /// the date and rev lead because they have fixed widths, so they line up
    /// even when one long url would push trailing columns off screen
    fn source(&self, source: &LockedSource) -> String {
        let date = source.last_modified.map(render::date).unwrap_or_default();
        let rev = render::printable(source.rev.as_deref().unwrap_or_default());
        format!(
            "{}  {rev:<7}  {}",
            self.style.paint(Sgr::Dim, &format!("{date:<10}")),
            render::printable(&source.url)
        )
    }
}

fn followed_pins(inputs: &[TreeInput]) -> Option<String> {
    let follows = inputs
        .iter()
        .filter_map(|input| {
            let TreeTarget::FollowsPin(ref pin) = input.target else {
                return None;
            };
            let name = render::printable(&input.name);
            Some(if *pin == name {
                name
            } else {
                format!("{name} {ARROW} {pin}")
            })
        })
        .collect::<Vec<_>>();
    (!follows.is_empty()).then(|| format!("follows {}", follows.join(", ")))
}

/// the widest name end among inputs that print their own line, where `start`
/// is the column their names begin at
fn name_column(inputs: &[TreeInput], start: usize) -> usize {
    inputs
        .iter()
        .filter_map(|input| {
            let own = start + render::printable(&input.name).chars().count();
            match input.target {
                TreeTarget::Locked {
                    inputs: ref children,
                    ..
                } => Some(own.max(name_column(children, start + GLYPH_WIDTH))),
                TreeTarget::Repeated(_) => Some(own),
                TreeTarget::Unknown(_)
                | TreeTarget::FollowsInput(_)
                | TreeTarget::FollowsPin(_) => None,
            }
        })
        .max()
        .unwrap_or(0)
}

/// every pin that pulls in its own copy of a source, so a copy can point at
/// the other pins an `[all_follow]` rule would fold it into
struct Copies<'a> {
    pins: BTreeMap<&'a LockedSource, BTreeSet<&'a str>>,
}

impl<'a> Copies<'a> {
    fn of(trees: &'a [PinTree]) -> Self {
        fn collect<'a>(
            pin: &'a str,
            inputs: &'a [TreeInput],
            pins: &mut BTreeMap<&'a LockedSource, BTreeSet<&'a str>>,
        ) {
            for input in inputs {
                if let TreeTarget::Locked {
                    ref source,
                    inputs: ref children,
                } = input.target
                {
                    pins.entry(source).or_default().insert(pin);
                    collect(pin, children, pins);
                }
            }
        }

        let mut pins = BTreeMap::new();
        for tree in trees {
            collect(&tree.name, &tree.inputs, &mut pins);
        }
        Self { pins }
    }

    fn elsewhere(&self, source: &LockedSource, pin: &str) -> Vec<&str> {
        self.pins
            .get(source)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&other| other != pin)
            .collect()
    }
}
