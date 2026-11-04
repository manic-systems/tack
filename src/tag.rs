// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::HashMap,
    fmt::{
        Display,
        Formatter,
        Result as FmtResult,
    },
    str::FromStr,
};

use misstep::Result;

use crate::{
    error::user_bail,
    fetch,
    lock::LockedTag,
    source::Source,
};

/// a tag name with `{version}`, integers joined by `.`, `-` or `_`, and named
/// placeholders like `{commit}` matching letters and digits
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagTemplate {
    pieces:     Vec<Piece>,
    version_at: usize,
    /// a trailing `*`, matching anything after the last piece
    open_end:   bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    Text(String),
    Version,
    Named(String),
}

impl Display for Piece {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match *self {
            Self::Text(ref text) => f.write_str(text),
            Self::Version => f.write_str("{version}"),
            Self::Named(ref name) => write!(f, "{{{name}}}"),
        }
    }
}

impl FromStr for TagTemplate {
    type Err = misstep::Report;

    fn from_str(raw: &str) -> Result<Self> {
        if raw.contains(['&', '#', '?']) {
            user_bail!("tag template '{raw}' cannot contain &, # or ?");
        }
        let (body, open_end) = raw
            .strip_suffix('*')
            .map_or((raw, false), |closed| (closed, true));
        if body.contains('*') {
            user_bail!("tag template '{raw}' can only use * at its end");
        }

        let mut pieces = Vec::new();
        let mut rest = body;
        while let Some((text, opened)) = rest.split_once('{') {
            let Some((name, after)) = opened.split_once('}') else {
                user_bail!("tag template '{raw}' has a {{ with no closing }}");
            };
            let piece = match name {
                "version" => Piece::Version,
                "tag" => user_bail!("tag template '{raw}' cannot have a {{tag}} placeholder"),
                _ if !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') =>
                {
                    Piece::Named(name.to_owned())
                },
                _ => {
                    user_bail!(
                        "tag template '{raw}' has a placeholder name that isn't letters, digits \
                         or _"
                    )
                },
            };
            if pieces.contains(&piece) {
                user_bail!("tag template '{raw}' has more than one {piece}");
            }
            if !text.is_empty() {
                pieces.push(Piece::Text(text.to_owned()));
            } else if let Some(last) = pieces.last() {
                user_bail!("tag template '{raw}' needs text between {last} and {piece}");
            }
            pieces.push(piece);
            rest = after;
        }
        if !rest.is_empty() {
            pieces.push(Piece::Text(rest.to_owned()));
        }
        let crowded = pieces.windows(2).find_map(|pair| {
            match *pair {
                [ref named @ Piece::Named(_), Piece::Text(ref text)]
                    if text.starts_with(|ch: char| ch.is_ascii_alphanumeric()) =>
                {
                    Some(named)
                },
                _ => None,
            }
        });
        if let Some(named) = crowded {
            user_bail!("tag template '{raw}' has a letter or digit right after {named}");
        }

        let Some(version_at) = pieces.iter().position(|piece| *piece == Piece::Version) else {
            user_bail!("tag template '{raw}' needs a {{version}} placeholder");
        };
        Ok(Self {
            pieces,
            version_at,
            open_end,
        })
    }
}

impl Display for TagTemplate {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        for piece in &self.pieces {
            write!(f, "{piece}")?;
        }
        let star = if self.open_end { "*" } else { "" };
        f.write_str(star)
    }
}

impl TagTemplate {
    /// the matching tags and their versions, newest first
    fn ranked<'tag>(
        &self,
        tags: impl IntoIterator<Item = &'tag str>,
    ) -> Vec<(Vec<u64>, &'tag str)> {
        let mut matching = tags
            .into_iter()
            .filter_map(|tag| Some((self.version(tag)?, tag)))
            .collect::<Vec<_>>();
        matching.sort_unstable_by(|left, right| right.cmp(left));
        matching
    }

    /// `{version}` for asset urls, including a standalone `x.y.` prefix from
    /// templates like `v0.60.{version}`
    fn asset_version<'tag>(&self, tag: &'tag str) -> &'tag str {
        let Some(parts) = self.parts(tag) else {
            return tag;
        };
        let start = parts[..self.version_at]
            .iter()
            .map(|part| part.len())
            .sum::<usize>();
        let end = start + parts[self.version_at].len();
        let literal = match self.version_at.checked_sub(1).map(|at| &self.pieces[at]) {
            Some(&Piece::Text(ref text)) => text.as_str(),
            Some(&(Piece::Version | Piece::Named(_))) | None => "",
        };
        let mut head = literal;
        while let Some(before_dot) = head.strip_suffix('.') {
            let digits = before_dot.trim_end_matches(|ch: char| ch.is_ascii_digit());
            if digits.len() == before_dot.len() {
                break;
            }
            head = digits;
        }
        let lead = head.strip_suffix(['v', 'V']).unwrap_or(head);
        let literal_at = start - literal.len();
        let (before_lead, _) = tag.split_at(literal_at + lead.len());
        let from = if before_lead.ends_with(|ch: char| ch.is_ascii_alphanumeric()) {
            start
        } else {
            literal_at + head.len()
        };
        tag.get(from..end).unwrap_or(tag)
    }

    /// the tags in `text`, skipping matches that `runs_into` from either side
    fn scan(&self, text: &str) -> Vec<String> {
        let (anchor, starts_open) = match self.pieces[0] {
            Piece::Text(ref literal) => (literal.as_str(), false),
            Piece::Version => ("", true),
            Piece::Named(_) => ("", false),
        };
        let ends_open = self.pieces.last() == Some(&Piece::Version);
        let fits = |after: &str| !runs_into(after.chars(), ends_open);
        let mut found = text
            .match_indices(anchor)
            .filter_map(|(at, _)| {
                let (before, from_anchor) = text.split_at(at);
                if runs_into(before.chars().rev(), starts_open) {
                    return None;
                }
                self.split(from_anchor, &fits).map(|parts| parts.concat())
            })
            .collect::<Vec<_>>();
        found.sort_unstable();
        found.dedup();
        found
    }

    fn version(&self, tag: &str) -> Option<Vec<u64>> {
        version_parts(self.parts(tag)?[self.version_at])
    }

    fn parts<'tag>(&self, tag: &'tag str) -> Option<Vec<&'tag str>> {
        self.split(tag, &|rest: &str| self.open_end || rest.is_empty())
    }

    fn split<'text>(
        &self,
        text: &'text str,
        fits: &impl Fn(&str) -> bool,
    ) -> Option<Vec<&'text str>> {
        let mut parts = Vec::with_capacity(self.pieces.len());
        split_into(&self.pieces, text, fits, &mut parts).then_some(parts)
    }

    fn fill(&self, url: &str, tag: &str) -> String {
        let parts = self.parts(tag).unwrap_or_default();
        let value = |name: &str| {
            match name {
                "tag" => Some(tag),
                "version" => Some(self.asset_version(tag)),
                _ => {
                    self.pieces.iter().zip(&parts).find_map(|(piece, &part)| {
                        matches!(*piece, Piece::Named(ref named) if named == name).then_some(part)
                    })
                },
            }
        };
        let mut filled = String::with_capacity(url.len());
        let mut rest = url;
        while let Some((before, opened)) = rest.split_once('{') {
            filled.push_str(before);
            let named = opened
                .split_once('}')
                .and_then(|(name, after)| Some((value(name)?, after)));
            if let Some((text, after)) = named {
                filled.push_str(text);
                rest = after;
            } else {
                filled.push('{');
                rest = opened;
            }
        }
        filled.push_str(rest);
        filled
    }

    pub fn unknown_placeholder<'url>(&self, url: &'url str) -> Option<&'url str> {
        placeholders(url).find(|&name| {
            !matches!(name, "tag" | "version")
                && !self.pieces.contains(&Piece::Named(name.to_owned()))
        })
    }
}

fn split_into<'text>(
    pieces: &[Piece],
    text: &'text str,
    fits: &impl Fn(&str) -> bool,
    parts: &mut Vec<&'text str>,
) -> bool {
    let Some((piece, later)) = pieces.split_first() else {
        return fits(text);
    };
    let run = |within: fn(char) -> bool| text.find(|ch| !within(ch)).unwrap_or(text.len());
    let lens = match *piece {
        Piece::Text(ref literal) if text.starts_with(literal.as_str()) => {
            literal.len()..=literal.len()
        },
        Piece::Text(_) => return false,
        Piece::Version => 1..=run(|ch| ch.is_ascii_digit() || SEPARATORS.contains(&ch)),
        Piece::Named(_) => 1..=run(|ch| ch.is_ascii_alphanumeric()),
    };
    for len in lens.rev() {
        let (part, after) = text.split_at(len);
        if *piece == Piece::Version && version_parts(part).is_none() {
            continue;
        }
        parts.push(part);
        if split_into(later, after, fits, parts) {
            return true;
        }
        parts.pop();
    }
    false
}

fn placeholders(url: &str) -> impl Iterator<Item = &str> {
    url.split('{')
        .skip(1)
        .filter_map(|opened| opened.split_once('}').map(|(name, _)| name))
}

/// where a fixed pin reads its tags, a repo or an http(s) page
#[derive(Clone, Debug)]
pub enum TagSource {
    Repo(Source),
    Page(String),
}

impl TagSource {
    pub fn parse(name: &str, expanded: &str) -> Result<Self> {
        match expanded.parse::<Source>()? {
            Source::Tarball { url } => Ok(Self::Page(url)),
            Source::Path { .. } => {
                user_bail!("input '{name}': tags_from needs a repo or an http(s) page, not a path")
            },
            Source::Github { .. } | Source::Gitlab { .. } | Source::Git { .. } => {
                followable(name, expanded).map(Self::Repo)
            },
        }
    }

    fn ranked(&self, template: &TagTemplate, window: usize) -> Result<Vec<String>> {
        match *self {
            Self::Repo(ref source) => ranked_in_repo(source, template, window),
            Self::Page(ref url) => {
                let tags = template.scan(&fetch::raw(url, None)?);
                let ranked = template.ranked(tags.iter().map(String::as_str));
                Ok(ranked.into_iter().map(|(_, tag)| tag.to_owned()).collect())
            },
        }
    }
}

/// a repo's tags that `template` matches, newest first, where tags tying on
/// version within the first `window` go by commit date
fn ranked_in_repo(source: &Source, template: &TagTemplate, window: usize) -> Result<Vec<String>> {
    let tags = fetch::list_tags(source)?;
    let mut ranked = template.ranked(tags.iter().map(|tag| tag.name.as_str()));

    let mut groups = Vec::new();
    let mut start = 0;
    while start < ranked.len().min(window) {
        let version = &ranked[start].0;
        let len = ranked[start..]
            .iter()
            .take_while(|&&(ref other, _)| other == version)
            .count();
        if len > 1 {
            groups.push(start..start + len);
        }
        start += len;
    }

    if !groups.is_empty() {
        let commits = tags
            .iter()
            .map(|tag| (tag.name.as_str(), tag.commit.as_str()))
            .collect::<HashMap<_, _>>();
        let tied = groups
            .iter()
            .flat_map(|group| ranked[group.clone()].iter().map(|&(_, tag)| tag))
            .collect::<Vec<_>>();
        let revs = tied.iter().map(|tag| commits[tag]).collect::<Vec<_>>();
        if let Some(stamps) = fetch::commit_times(source, &revs) {
            let dated = tied.into_iter().zip(stamps).collect::<HashMap<_, _>>();
            for group in groups {
                ranked[group].sort_by_key(|&(_, tag)| Reverse(dated[tag]));
            }
        }
    }

    Ok(ranked.into_iter().map(|(_, tag)| tag.to_owned()).collect())
}

/// a tag template and where its tags come from, [`None`] for the url's own repo
#[derive(Clone, Debug)]
pub struct TagFollow {
    template: TagTemplate,
    from:     Option<TagSource>,
}

impl TagFollow {
    pub fn new(name: &str, template: TagTemplate, from: Option<TagSource>) -> Result<Self> {
        if template.open_end && matches!(from, Some(TagSource::Page(_))) {
            user_bail!(
                "input '{name}': tag template '{template}' cannot end in * when reading a page"
            );
        }
        Ok(Self { template, from })
    }

    pub fn locked(&self, tag: String) -> LockedTag {
        let version = self.template.asset_version(&tag).to_owned();
        LockedTag { tag, version }
    }
}

const SEPARATORS: [char; 3] = ['.', '-', '_'];

fn version_parts(middle: &str) -> Option<Vec<u64>> {
    middle
        .split(SEPARATORS)
        .map(|part| {
            let numeric = !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
            numeric.then(|| part.parse::<u64>().ok()).flatten()
        })
        .collect()
}

/// whether `beyond` continues a match, with a letter or digit, or at an `open`
/// edge with a separator and digit
fn runs_into(mut beyond: impl Iterator<Item = char>, open: bool) -> bool {
    match beyond.next() {
        Some(ch) if ch.is_alphanumeric() => true,
        Some(ch) if open && SEPARATORS.contains(&ch) => {
            beyond.next().is_some_and(|next| next.is_ascii_digit())
        },
        Some(_) | None => false,
    }
}

pub struct Followed<'url> {
    pub url: Cow<'url, str>,
    pub tag: Option<String>,
}

/// rewrites `expanded` to the newest tag `follow` matches, or passes it
/// through untouched for a pin with no template
pub fn follow<'url>(
    name: &str,
    follow: Option<&TagFollow>,
    expanded: &'url str,
) -> Result<Followed<'url>> {
    let Some(tagged) = follow else {
        return Ok(Followed {
            url: Cow::Borrowed(expanded),
            tag: None,
        });
    };
    let tag_template = &tagged.template;
    if names_asset(expanded) {
        return follow_asset(name, tagged, expanded);
    }
    let source = followable(name, expanded)?;
    let Some(tag) = ranked_in_repo(&source, tag_template, 1)?.into_iter().next() else {
        user_bail!("input '{name}': no tag matches {tag_template}");
    };
    let (base, fragment) = expanded
        .find('#')
        .map_or((expanded, ""), |at| expanded.split_at(at));
    let separator = if base.contains('?') { '&' } else { '?' };
    Ok(Followed {
        url: Cow::Owned(format!("{base}{separator}ref=refs/tags/{tag}{fragment}")),
        tag: Some(tag),
    })
}

/// the source of a pin url a template can follow, which names no ref or rev
/// of its own
pub fn followable(name: &str, expanded: &str) -> Result<Source> {
    let source = expanded.parse::<Source>()?;
    let (reff, rev) = match source {
        Source::Github {
            ref reff, ref rev, ..
        }
        | Source::Gitlab {
            ref reff, ref rev, ..
        }
        | Source::Git {
            ref reff, ref rev, ..
        } => (reff, rev),
        Source::Tarball { .. } | Source::Path { .. } => {
            user_bail!("input '{name}': tag following needs a github, gitlab, or git url")
        },
    };
    if let Source::Git { ref url, .. } = source
        && fetch::is_local_url(url)
    {
        user_bail!("input '{name}': tag following needs a network remote, not a local file url");
    }
    if reff.is_some() || rev.is_some() {
        user_bail!("input '{name}': drop the ref or rev from its url, the tag picks the rev");
    }
    Ok(source)
}

/// how many of the newest matching tags to try for a release asset, since a
/// release can be tagged before its assets are uploaded, and some tags never
/// get a release at all
const ASSET_TRIES: usize = 10;

/// whether a fixed pin's url names its release asset with placeholders like
/// `{tag}`
pub fn names_asset(expanded: &str) -> bool {
    placeholders(expanded).next().is_some()
}

/// the repo whose tags pick a release asset, from a url like
/// `https://github.com/o/r/releases/download/{tag}/x.tar.gz`, which GitHub,
/// Forgejo, Gitea and GitLab (`/-/releases/`) all share up to `releases`
pub fn asset_repo(name: &str, expanded: &str) -> Result<Source> {
    let found = expanded
        .strip_prefix("https://")
        .and_then(|rest| {
            rest.split_once("/-/releases/")
                .or_else(|| rest.split_once("/releases/"))
        })
        .map(|(repo, _)| repo)
        .filter(|repo| repo.matches('/').count() >= 2 && !repo.contains('{'));
    let Some(repo) = found else {
        user_bail!(
            "input '{name}': a fixed pin follows tags through a release download url, like \
             https://github.com/o/r/releases/download/{{tag}}/file.tar.gz"
        );
    };
    format!("git+https://{repo}").parse::<Source>()
}

fn follow_asset(name: &str, follow: &TagFollow, expanded: &str) -> Result<Followed<'static>> {
    let template = &follow.template;
    let ranked = match follow.from {
        Some(ref from) => from.ranked(template, ASSET_TRIES)?,
        None => ranked_in_repo(&asset_repo(name, expanded)?, template, ASSET_TRIES)?,
    };
    if ranked.is_empty() {
        user_bail!("input '{name}': no tag matches {template}");
    }
    for tag in ranked.iter().take(ASSET_TRIES) {
        let url = template.fill(expanded, tag);
        if fetch::serves(&url) {
            return Ok(Followed {
                url: Cow::Owned(url),
                tag: Some(tag.to_owned()),
            });
        }
    }
    user_bail!(
        "input '{name}': none of the newest {} tags matching {template} has the asset {expanded}",
        ranked.len().min(ASSET_TRIES)
    )
}
