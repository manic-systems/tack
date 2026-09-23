// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    collections::BTreeMap,
    ffi::OsStr,
    fmt::{
        Display,
        Formatter,
        Result as FmtResult,
    },
    fs,
    io::ErrorKind,
    os::unix::{
        ffi::OsStrExt as _,
        fs::PermissionsExt as _,
    },
    path::{
        Component,
        Path,
        PathBuf,
    },
};

use diffy::{
    Hunk,
    Line,
    Patch,
    apply_bytes,
    patch_set::{
        FileMode,
        FileOperation,
        FilePatch,
        ParseOptions,
        PatchKind,
        PatchSet,
    },
};
use misstep::{
    Result,
    ResultExt as _,
};

use crate::error::user_bail;

pub struct SourceTree {
    root: PathBuf,
}

/// a patch's file changes, staged so one failing file leaves the tree as it
/// was
enum Staged {
    Written {
        contents: Vec<u8>,
        mode:     FileMode,
    },
    Removed,
}

/// how a file's hunks land in the tree
enum Edit {
    /// patched where it is, created first when `creatable` and the hunks
    /// only add lines to an empty file
    InPlace {
        creatable: bool,
    },
    Rename,
    Copy,
}

impl SourceTree {
    pub fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            root: root.canonicalize()?,
        })
    }

    pub fn apply(&self, patch: &[u8]) -> Result<()> {
        let text = recounted(patch)?;
        let mut staged = BTreeMap::new();
        for file_patch in file_patches(&text)? {
            self.stage(&mut staged, &file_patch, false)?;
        }
        // removals go first so a file can take the place of a directory the
        // patch empties
        for (target, _) in staged
            .iter()
            .filter(|&(_, entry)| matches!(*entry, Staged::Removed))
        {
            self.remove(target)?;
        }
        for (target, entry) in &staged {
            if let Staged::Written { ref contents, mode } = *entry {
                self.write(target, contents, mode)?;
            }
        }
        Ok(())
    }

    /// GNU patch's test for a patch that is already in the tree: its reverse
    /// applies cleanly
    pub fn already_applied(&self, patch: &[u8]) -> bool {
        let Ok(text) = recounted(patch) else {
            return false;
        };
        let mut staged = BTreeMap::new();
        file_patches(&text).is_ok_and(|parsed| {
            parsed
                .iter()
                .rev()
                .all(|file_patch| self.stage(&mut staged, file_patch, true).is_ok())
        })
    }

    /// the hunks of `patch` that don't apply to the tree as it stands, kept as
    /// a patch of their own, like the `.rej` files GNU patch leaves
    pub fn rejects(&self, patch: &[u8]) -> Vec<u8> {
        let Ok(text) = recounted(patch) else {
            return Vec::new();
        };
        let mut current = BTreeMap::<PathBuf, Vec<u8>>::new();
        let mut rejected = Vec::new();
        for section in sections(&text) {
            let [original, modified] = [section.path(b"--- "), section.path(b"+++ ")]
                .map(|path| self.resolve(&path?).ok());
            let chosen = [&original, &modified]
                .into_iter()
                .flatten()
                .find(|target| current.contains_key(*target) || target.exists())
                .or(original.as_ref())
                .or(modified.as_ref());
            let mut base = chosen
                .and_then(|target| {
                    current
                        .get(target)
                        .cloned()
                        .or_else(|| fs::read(target).ok())
                })
                .unwrap_or_default();
            let mut failed = Vec::new();
            for hunk in &section.hunks {
                let single = [section.paths, hunk].concat();
                let applied = Patch::from_bytes(&single)
                    .ok()
                    .and_then(|parsed| patched_contents(b"", &base, &parsed).ok());
                match applied {
                    Some(next) => base = next,
                    None => failed.push(*hunk),
                }
            }
            if !failed.is_empty() {
                rejected.extend_from_slice(section.header);
                rejected.extend(failed.concat());
            }
            for target in chosen.into_iter().chain(&modified) {
                current.insert(target.clone(), base.clone());
            }
        }
        rejected
    }

    fn stage(
        &self,
        staged: &mut BTreeMap<PathBuf, Staged>,
        file_patch: &FilePatch<'_, [u8]>,
        reversed: bool,
    ) -> Result<()> {
        let PatchKind::Text(ref forward) = *file_patch.patch() else {
            user_bail!("binary patches are not supported");
        };
        let unsupported = [file_patch.old_mode(), file_patch.new_mode()]
            .into_iter()
            .flatten()
            .any(|mode| matches!(*mode, FileMode::Symlink | FileMode::Gitlink));
        if unsupported {
            user_bail!("symlink and submodule patches are not supported");
        }
        let reverse;
        let hunks = if reversed {
            reverse = forward.reverse();
            &reverse
        } else {
            forward
        };
        let new_mode = file_patch.new_mode().copied();
        // git writes rename and copy paths without the a/ b/ prefix
        let operation = match *file_patch.operation() {
            ref unprefixed @ (FileOperation::Rename { .. } | FileOperation::Copy { .. }) => {
                unprefixed.clone()
            },
            FileOperation::Create(ref path) => {
                FileOperation::Create(Cow::Borrowed(unprefixed(path)))
            },
            FileOperation::Delete(ref path) => {
                FileOperation::Delete(Cow::Borrowed(unprefixed(path)))
            },
            FileOperation::Modify {
                ref original,
                ref modified,
            } => {
                FileOperation::Modify {
                    original: Cow::Borrowed(unprefixed(original)),
                    modified: Cow::Borrowed(unprefixed(modified)),
                }
            },
        };
        match (operation, reversed) {
            (FileOperation::Create(ref path), false) | (FileOperation::Delete(ref path), true) => {
                let target = self.resolve(path)?;
                if self.current(staged, &target)?.is_some() {
                    user_bail!("{} already exists", String::from_utf8_lossy(path));
                }
                let contents = patched_contents(path, &[], hunks)?;
                staged.insert(target, Staged::Written {
                    contents,
                    mode: new_mode.unwrap_or(FileMode::Regular),
                });
            },
            (FileOperation::Delete(ref path), false) | (FileOperation::Create(ref path), true) => {
                let target = self.resolve(path)?;
                let Some((contents, _)) = self.current(staged, &target)? else {
                    user_bail!("delete {}: no such file", String::from_utf8_lossy(path));
                };
                if !patched_contents(path, &contents, hunks)?.is_empty() {
                    user_bail!(
                        "delete {}: the file differs from the patch",
                        String::from_utf8_lossy(path)
                    );
                }
                staged.insert(target, Staged::Removed);
            },
            (
                FileOperation::Modify {
                    ref original,
                    ref modified,
                },
                _,
            ) => {
                let original_exists = self.current(staged, &self.resolve(original)?)?.is_some();
                let modified_exists = self.current(staged, &self.resolve(modified)?)?.is_some();
                let chosen = if original_exists || !modified_exists {
                    original
                } else {
                    modified
                };
                let edit = Edit::InPlace {
                    creatable: original == modified,
                };
                self.stage_edit(staged, chosen, chosen, hunks, new_mode, &edit)?;
            },
            (FileOperation::Rename { ref from, ref to }, false)
            | (
                FileOperation::Rename {
                    from: ref to,
                    to: ref from,
                },
                true,
            ) => self.stage_edit(staged, from, to, hunks, new_mode, &Edit::Rename)?,
            (FileOperation::Copy { ref from, ref to }, false) => {
                self.stage_edit(staged, from, to, hunks, new_mode, &Edit::Copy)?;
            },
            (FileOperation::Copy { ref to, .. }, true) => {
                let edit = Edit::InPlace { creatable: false };
                self.stage_edit(staged, to, to, hunks, new_mode, &edit)?;
            },
        }
        Ok(())
    }

    fn stage_edit(
        &self,
        staged: &mut BTreeMap<PathBuf, Staged>,
        from: &[u8],
        to: &[u8],
        hunks: &Patch<'_, [u8]>,
        new_mode: Option<FileMode>,
        edit: &Edit,
    ) -> Result<()> {
        let source = self.resolve(from)?;
        let target = self.resolve(to)?;
        if source != target && self.current(staged, &target)?.is_some() {
            user_bail!("{} already exists", String::from_utf8_lossy(to));
        }
        // hand-written patches often create a file as `--- a/x` against `@@ -0,0`
        let creates = matches!(*edit, Edit::InPlace { creatable: true })
            && !hunks.hunks().is_empty()
            && hunks
                .hunks()
                .iter()
                .all(|hunk| hunk.old_range().start() == 0 && hunk.old_range().is_empty());
        let current = self
            .current(staged, &source)?
            .or_else(|| creates.then(|| (Vec::new(), FileMode::Regular)));
        let Some((base, mode)) = current else {
            user_bail!("read {}: no such file", String::from_utf8_lossy(from));
        };
        let contents = patched_contents(from, &base, hunks)?;
        if matches!(*edit, Edit::Rename) && source != target {
            staged.insert(source, Staged::Removed);
        }
        staged.insert(target, Staged::Written {
            contents,
            mode: new_mode.unwrap_or(mode),
        });
        Ok(())
    }

    fn current(
        &self,
        staged: &BTreeMap<PathBuf, Staged>,
        target: &Path,
    ) -> Result<Option<(Vec<u8>, FileMode)>> {
        match staged.get(target) {
            Some(&Staged::Written { ref contents, mode }) => {
                return Ok(Some((contents.clone(), mode)));
            },
            Some(&Staged::Removed) => return Ok(None),
            None => {},
        }
        match fs::read(target) {
            Ok(contents) => Ok(Some((contents, mode_of(target)?))),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) if err.kind() == ErrorKind::IsADirectory && emptied(staged, target)? => {
                Ok(None)
            },
            Err(err)
                if err.kind() == ErrorKind::NotADirectory
                    && target
                        .ancestors()
                        .any(|dir| matches!(staged.get(dir), Some(&Staged::Removed))) =>
            {
                Ok(None)
            },
            Err(err) => {
                let relative = target.strip_prefix(&self.root).unwrap_or(target);
                user_bail!("read {}: {err}", relative.display())
            },
        }
    }

    fn resolve(&self, raw: &[u8]) -> Result<PathBuf> {
        let relative = Path::new(OsStr::from_bytes(raw));
        let contained = relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
        if !contained {
            user_bail!(
                "patch touches {}, which is outside the tree",
                relative.display()
            );
        }
        let mut walked = self.root.clone();
        for component in relative.components() {
            walked.push(component);
            match fs::symlink_metadata(&walked) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    user_bail!("patch touches {}, which is a symlink", relative.display());
                },
                Err(err) if err.kind() == ErrorKind::NotFound => break,
                _ => {},
            }
        }
        Ok(self.root.join(relative))
    }

    /// deletes `target` and then every directory above it that this leaves
    /// empty, so none of them end up in the NAR
    fn remove(&self, target: &Path) -> Result<()> {
        writable(target.parent().unwrap_or(&self.root))?;
        match fs::remove_file(target) {
            Ok(()) => {},
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| format!("delete {}", target.display()));
            },
        }
        for dir in target
            .ancestors()
            .skip(1)
            .take_while(|&dir| dir != self.root)
        {
            if fs::read_dir(dir)?.next().is_some() {
                break;
            }
            writable(dir.parent().unwrap_or(&self.root))?;
            fs::remove_dir(dir).with_context(|| format!("delete {}", dir.display()))?;
        }
        Ok(())
    }

    fn write(&self, target: &Path, contents: &[u8], mode: FileMode) -> Result<()> {
        let parent = target.parent().unwrap_or(&self.root);
        if let Some(existing) = parent.ancestors().find(|dir| dir.exists()) {
            writable(existing)?;
        }
        fs::create_dir_all(parent)?;
        writable(target)?;
        fs::write(target, contents).with_context(|| format!("write {}", target.display()))?;
        let bits = if mode == FileMode::Executable {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(target, fs::Permissions::from_mode(bits))?;
        Ok(())
    }
}

/// whether every file under `dir` is staged for removal
fn emptied(staged: &BTreeMap<PathBuf, Staged>, dir: &Path) -> Result<bool> {
    for listed in fs::read_dir(dir)? {
        let entry = listed?;
        let path = entry.path();
        let gone = if entry.file_type()?.is_dir() {
            emptied(staged, &path)?
        } else {
            matches!(staged.get(&path), Some(&Staged::Removed))
        };
        if !gone {
            return Ok(false);
        }
    }
    Ok(true)
}

/// tarballs can unpack read-only, and since the NAR keeps only the executable
/// bit, adding the write bit changes nothing in the result
fn writable(path: &Path) -> Result<()> {
    let mode = match fs::metadata(path) {
        Ok(meta) => meta.permissions().mode(),
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    if mode & 0o200 == 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200))?;
    }
    Ok(())
}

fn unprefixed(path: &[u8]) -> &[u8] {
    path.strip_prefix(b"a/")
        .or_else(|| path.strip_prefix(b"b/"))
        .unwrap_or(path)
}

/// a path git wrote C-quoted, like `"a/docs/\346\227\245.md"`
fn unquoted(quoted: &[u8]) -> Option<Vec<u8>> {
    let mut bytes = quoted.strip_prefix(b"\"")?.iter().copied();
    let mut path = Vec::new();
    loop {
        match bytes.next()? {
            b'"' => return Some(path),
            b'\\' => {
                let escaped = match bytes.next()? {
                    b'a' => 0x07,
                    b'b' => 0x08,
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'v' => 0x0B,
                    b'f' => 0x0C,
                    b'r' => b'\r',
                    digit @ b'0'..=b'7' => {
                        let octal = [digit, bytes.next()?, bytes.next()?];
                        u8::from_str_radix(str::from_utf8(&octal).ok()?, 8).ok()?
                    },
                    other => other,
                };
                path.push(escaped);
            },
            other => path.push(other),
        }
    }
}

/// one file's part of a patch, split up by text so single hunks can be tried
struct Section<'a> {
    /// everything before the first hunk, `diff --git` and index lines included
    header: &'a [u8],
    /// just the `---` and `+++` lines, which is all a single-file patch needs
    paths:  &'a [u8],
    hunks:  Vec<&'a [u8]>,
}

impl Section<'_> {
    /// the unprefixed path on the line starting with `prefix`, [`None`] for
    /// `/dev/null`
    fn path(&self, prefix: &[u8]) -> Option<Vec<u8>> {
        let named = self
            .paths
            .split(|byte| *byte == b'\n')
            .find_map(|line| line.strip_prefix(prefix))?;
        let path = if named.starts_with(b"\"") {
            unquoted(named)?
        } else {
            named
                .split(|byte| *byte == b'\t')
                .next()?
                .trim_ascii_end()
                .to_vec()
        };
        (path != b"/dev/null").then(|| unprefixed(&path).to_vec())
    }
}

fn starts_file(lines: &[&[u8]], index: usize) -> bool {
    let line = lines[index];
    line.starts_with(b"diff ")
        || (line.starts_with(b"--- ")
            && lines
                .get(index + 1)
                .is_some_and(|next| next.starts_with(b"+++ ")))
}

fn is_blank(line: &[u8]) -> bool {
    line == b"\n" || line == b"\r\n"
}

fn is_hunk_line(line: &[u8]) -> bool {
    matches!(line.first(), Some(b' ' | b'+' | b'-' | b'\\')) || is_blank(line)
}

/// the `-- ` line git format-patch closes a mail with, ahead of its version
fn is_signature(lines: &[&[u8]], index: usize) -> bool {
    matches!(lines[index], b"-- \n" | b"--\n")
        && lines.get(index + 1).is_none_or(|next| !is_hunk_line(next))
}

fn ends_hunk(lines: &[&[u8]], index: usize) -> bool {
    lines.get(index).is_none_or(|line| !is_hunk_line(line))
        || starts_file(lines, index)
        || is_signature(lines, index)
}

#[derive(PartialEq, Eq)]
struct Counts {
    old: usize,
    new: usize,
}

impl Counts {
    const fn add(&mut self, line: &[u8]) {
        match line.first().copied() {
            Some(b'-') => self.old += 1,
            Some(b'+') => self.new += 1,
            Some(b'\\') => {},
            _ => {
                self.old += 1;
                self.new += 1;
            },
        }
    }
}

/// a `@@ -a,b +c,d @@` line taken apart
struct HunkHeader<'a> {
    old_start: usize,
    new_start: usize,
    counts:    Counts,
    /// whatever follows the closing `@@`, the line ending included
    rest:      &'a [u8],
}

/// where a hunk's body ends, and the line counts it has
struct HunkBody {
    end:       usize,
    counts:    Counts,
    /// the body falls short of the header by a different count on each side
    /// with no hunk after it, as when a download was cut off or a line lost
    /// its leading space, or it has no lines at all
    truncated: bool,
}

/// reads the body by the header's counts the way diffy does, which keeps
/// `--- x` and `+++ y` lines inside it, and only when those counts leave lines
/// behind or run out guesses the end from what the lines look like
fn hunk_body(lines: &[&[u8]], index: usize, header: &HunkHeader<'_>) -> HunkBody {
    let mut counted = Counts { old: 0, new: 0 };
    let mut end = index + 1;
    while (counted.old < header.counts.old || counted.new < header.counts.new)
        && lines
            .get(end)
            .is_some_and(|line| is_hunk_line(line) && !is_signature(lines, end))
    {
        counted.add(lines[end]);
        end += 1;
    }
    while lines.get(end).is_some_and(|line| line.starts_with(b"\\")) {
        end += 1;
    }
    let blanks = lines[end..]
        .iter()
        .take_while(|line| is_blank(line))
        .count();
    if counted == header.counts && ends_hunk(lines, end + blanks) {
        return HunkBody {
            end,
            counts: counted,
            truncated: false,
        };
    }
    // GNU patch reads a hunk short by the same count on both sides as one
    // missing trailing context, and an uneven shortfall as a broken patch
    // unless another hunk follows, where only the header was miscounted
    let uneven = header.counts.old.saturating_sub(counted.old)
        != header.counts.new.saturating_sub(counted.new);
    let next_hunk = lines.get(end).is_some_and(|line| line.starts_with(b"@@"));

    let mut stop = index + 1;
    while !ends_hunk(lines, stop) {
        stop += 1;
    }
    while stop > index + 1 && is_blank(lines[stop - 1]) {
        stop -= 1;
    }
    let mut counts = Counts { old: 0, new: 0 };
    for line in &lines[index + 1..stop] {
        counts.add(line);
    }
    let empty = counts == Counts { old: 0, new: 0 };
    HunkBody {
        end: stop,
        counts,
        truncated: (uneven && !next_hunk) || empty,
    }
}

fn sections(patch: &[u8]) -> Vec<Section<'_>> {
    let lines = patch
        .split_inclusive(|byte| *byte == b'\n')
        .collect::<Vec<_>>();
    let offsets = lines
        .iter()
        .scan(0, |offset, line| {
            let start = *offset;
            *offset += line.len();
            Some(start)
        })
        .chain([patch.len()])
        .collect::<Vec<_>>();
    let span = |from: usize, to: usize| &patch[offsets[from]..offsets[to]];

    let mut sections = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if !starts_file(&lines, index) {
            index += 1;
            continue;
        }
        let start = index;
        // a git header carries its own `---` and `+++` lines after the mode
        // and rename ones, so only the next `diff` line ends it early
        let git = lines[start].starts_with(b"diff --git ");
        let ends_header = |line: usize| {
            lines[line].starts_with(b"@@")
                || if git {
                    lines[line].starts_with(b"diff ")
                } else {
                    starts_file(&lines, line)
                }
        };
        index += 1;
        while index < lines.len() && !ends_header(index) {
            index += 1;
        }
        let header = span(start, index);
        let Some(minus) = (start..index).find(|&line| lines[line].starts_with(b"--- ")) else {
            continue;
        };
        let mut hunks = Vec::new();
        while let Some(parsed) = lines.get(index).and_then(|line| hunk_header(line)) {
            let body = hunk_body(&lines, index, &parsed);
            hunks.push(span(index, body.end));
            index = body.end;
        }
        sections.push(Section {
            header,
            paths: span(minus, (minus + 2).min(index)),
            hunks,
        });
    }
    sections
}

/// every hunk header whose counts are off rewritten with the ones its body has,
/// with blank lines read as empty context the way GNU patch reads them, and a
/// patch whose every line ends in CRLF read with plain newlines as GNU patch
/// does
fn recounted(raw: &[u8]) -> Result<Vec<u8>> {
    let crlf = raw.ends_with(b"\r\n")
        && raw
            .split_inclusive(|byte| *byte == b'\n')
            .all(|line| line.ends_with(b"\r\n"));
    let unix;
    let patch = if crlf {
        unix = raw
            .split_inclusive(|byte| *byte == b'\n')
            .flat_map(|line| [&line[..line.len() - 2], b"\n"])
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        &*unix
    } else {
        raw
    };
    let lines = patch
        .split_inclusive(|byte| *byte == b'\n')
        .collect::<Vec<_>>();
    let mut out = Vec::with_capacity(patch.len());
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let Some(header) = hunk_header(line) else {
            out.extend_from_slice(line);
            index += 1;
            continue;
        };
        let body = hunk_body(&lines, index, &header);
        if body.truncated {
            user_bail!(
                "the hunk at `{}` doesn't match its line counts, the patch looks cut off or \
                 malformed",
                String::from_utf8_lossy(line.trim_ascii_end())
            );
        }
        if body.counts == header.counts {
            out.extend_from_slice(line);
        } else {
            out.extend_from_slice(
                format!(
                    "@@ -{},{} +{},{} @@",
                    header.old_start, body.counts.old, header.new_start, body.counts.new
                )
                .as_bytes(),
            );
            out.extend_from_slice(header.rest);
        }
        for body_line in &lines[index + 1..body.end] {
            if is_blank(body_line) {
                out.push(b' ');
            }
            out.extend_from_slice(body_line);
        }
        index = body.end;
    }
    Ok(out)
}

fn hunk_header(line: &[u8]) -> Option<HunkHeader<'_>> {
    let inner = line.strip_prefix(b"@@ -")?;
    let close = inner.windows(3).position(|window| window == b" @@")?;
    let (raw_ranges, rest) = inner.split_at(close);
    let ranges = str::from_utf8(raw_ranges).ok()?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |range: &str| {
        let (start, len) = range.split_once(',').unwrap_or((range, "1"));
        Some((start.parse::<usize>().ok()?, len.parse::<usize>().ok()?))
    };
    let (old_start, old_len) = range(old)?;
    let (new_start, new_len) = range(new)?;
    Some(HunkHeader {
        old_start,
        new_start,
        counts: Counts {
            old: old_len,
            new: new_len,
        },
        rest: &rest[3..],
    })
}

// diffy's git mode only starts a file at `diff --git`, so plain `diff -u`
// output would parse as empty, and plain sections in a git patch would vanish
fn file_patches(patch: &[u8]) -> Result<Vec<FilePatch<'_, [u8]>>> {
    let is_git = patch
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"diff --git "));
    if is_git
        && sections(patch)
            .iter()
            .any(|section| !section.header.starts_with(b"diff --git "))
    {
        user_bail!("the patch mixes git and plain diff sections");
    }
    let options = if is_git {
        ParseOptions::gitdiff()
    } else {
        ParseOptions::unidiff()
    };
    let mut parsed = Vec::new();
    for result in PatchSet::parse_bytes(patch, options) {
        match result {
            Ok(file_patch) => parsed.push(file_patch),
            Err(err) => user_bail!("malformed patch: {err}"),
        }
    }
    if parsed.is_empty() {
        user_bail!("no file changes found in the patch");
    }
    Ok(parsed)
}

fn patched_contents(path: &[u8], base: &[u8], hunks: &Patch<'_, [u8]>) -> Result<Vec<u8>> {
    let lines = base
        .split_inclusive(|byte| *byte == b'\n')
        .collect::<Vec<_>>();
    for (index, hunk) in hunks.hunks().iter().enumerate() {
        if let Some(anchor) = Anchor::of(hunk)
            && !anchor.holds(hunk, &lines)
        {
            user_bail!(
                "{}: hunk #{} only fits at the {anchor} of the file",
                String::from_utf8_lossy(path),
                index + 1
            );
        }
    }
    match apply_bytes(base, hunks) {
        Ok(contents) => Ok(contents),
        Err(err) => user_bail!("{}: {err}", String::from_utf8_lossy(path)),
    }
}

/// where git and GNU patch pin a hunk, which diffy would otherwise apply at
/// the first match anywhere in the file
enum Anchor {
    Start,
    End,
}

impl Anchor {
    /// a hunk at line 1 belongs at the start, and one with context before its
    /// change but none after belongs at the end, while a hunk with no context
    /// at all, as `diff -U0` writes it, can't say
    fn of(hunk: &Hunk<'_, [u8]>) -> Option<Self> {
        let lines = hunk.lines();
        let is_context = |line: &&Line<'_, [u8]>| matches!(**line, Line::Context(_));
        let leading = lines.iter().take_while(is_context).count();
        let trailing = lines.iter().rev().take_while(is_context).count();
        if leading == 0 && trailing == 0 {
            return None;
        }
        if hunk.old_range().start() <= 1 {
            return Some(Self::Start);
        }
        (trailing == 0).then_some(Self::End)
    }

    fn holds(&self, hunk: &Hunk<'_, [u8]>, lines: &[&[u8]]) -> bool {
        let before = hunk
            .lines()
            .iter()
            .filter_map(|line| {
                match *line {
                    Line::Context(text) | Line::Delete(text) => Some(text),
                    Line::Insert(_) => None,
                }
            })
            .collect::<Vec<_>>();
        match *self {
            Self::Start => lines.starts_with(&before),
            Self::End => lines.ends_with(&before),
        }
    }
}

impl Display for Anchor {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(match *self {
            Self::Start => "start",
            Self::End => "end",
        })
    }
}

fn mode_of(path: &Path) -> Result<FileMode> {
    let permissions = fs::metadata(path)?.permissions();
    Ok(if permissions.mode() & 0o111 == 0 {
        FileMode::Regular
    } else {
        FileMode::Executable
    })
}
