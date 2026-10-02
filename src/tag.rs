// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    fmt::{
        Display,
        Formatter,
        Result as FmtResult,
    },
    str::FromStr,
};

use misstep::{
    Result,
    ResultExt as _,
};
use regex::Regex;

use crate::{
    error::user_bail,
    fetch,
    source::Source,
};

#[cfg(test)]
#[path = "tag_tests.rs"]
mod tests;

/// a tag name with one `{version}` slot, which only matches integers joined
/// by `.`, `-` or `_`, so `v{version}` skips `v2.0.3-purple` and `v2.1-rc1`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagTemplate {
    prefix: String,
    suffix: String,
}

impl FromStr for TagTemplate {
    type Err = misstep::Report;

    fn from_str(raw: &str) -> Result<Self> {
        let Some((prefix, suffix)) = raw.split_once("{version}") else {
            user_bail!("tag template '{raw}' needs a {{version}} placeholder");
        };
        if suffix.contains("{version}") {
            user_bail!("tag template '{raw}' has more than one {{version}}");
        }
        if raw.contains(['&', '#', '?']) {
            user_bail!("tag template '{raw}' cannot contain &, # or ?");
        }
        Ok(Self {
            prefix: prefix.to_owned(),
            suffix: suffix.to_owned(),
        })
    }
}

impl Display for TagTemplate {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{}{{version}}{}", self.prefix, self.suffix)
    }
}

impl TagTemplate {
    fn newest<'tag>(&self, tags: &'tag [String]) -> Option<&'tag str> {
        self.ranked(tags).into_iter().next()
    }

    /// the matching tags, newest first
    fn ranked<'tag>(&self, tags: &'tag [String]) -> Vec<&'tag str> {
        let mut matching = tags
            .iter()
            .filter_map(|tag| Some((self.version(tag)?, tag.as_str())))
            .collect::<Vec<_>>();
        matching.sort_unstable_by(|left, right| right.cmp(left));
        matching.into_iter().map(|(_, tag)| tag).collect()
    }

    /// the version a release asset's name carries, the tag from its first
    /// digit up to the template's suffix, so `v0.60.{version}` gives `0.60.3`
    fn asset_version<'tag>(&self, tag: &'tag str) -> &'tag str {
        tag.strip_suffix(self.suffix.as_str())
            .unwrap_or(tag)
            .trim_start_matches(|ch: char| !ch.is_ascii_digit())
    }

    fn version(&self, tag: &str) -> Option<Vec<u64>> {
        let middle = tag
            .strip_prefix(self.prefix.as_str())?
            .strip_suffix(self.suffix.as_str())?;
        middle
            .split(['.', '-', '_'])
            .map(|part| {
                let numeric = !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
                numeric.then(|| part.parse::<u64>().ok()).flatten()
            })
            .collect()
    }
}

/// a webpage to read an asset pin's tags from, for upstreams that publish
/// release files without tagging a repo. every `regex` match names one tag
#[derive(Clone, Debug)]
pub struct TagPage {
    url:   String,
    regex: Regex,
}

impl TagPage {
    pub fn new(url: &str, pattern: &str) -> Result<Self> {
        Ok(Self {
            url:   url.to_owned(),
            regex: pattern
                .parse::<Regex>()
                .with_context(|| format!("tag_regex '{pattern}' is not a valid regex"))?,
        })
    }

    /// the tags the page serves, in the order it serves them
    fn tags(&self, name: &str) -> Result<Vec<String>> {
        let page = fetch::raw(&self.url, None)
            .with_context(|| format!("input '{name}': read tag page {}", self.url))?;
        let found = self
            .regex
            .find_iter(&page)
            .map(|matched| matched.as_str().to_owned())
            .collect::<Vec<_>>();
        if found.is_empty() {
            user_bail!(
                "input '{name}': {} serves nothing matching {}",
                self.url,
                self.regex
            );
        }
        Ok(found)
    }
}

pub struct Followed<'url> {
    pub url: Cow<'url, str>,
    pub tag: Option<String>,
}

/// rewrites `expanded` to the newest tag `template` matches, or passes it
/// through untouched for a pin with no template. `page` replaces the repo
/// behind the url as the source of the tags it ranks
pub fn follow<'url>(
    name: &str,
    template: Option<&TagTemplate>,
    page: Option<&TagPage>,
    expanded: &'url str,
) -> Result<Followed<'url>> {
    let Some(tag_template) = template else {
        return Ok(Followed {
            url: Cow::Borrowed(expanded),
            tag: None,
        });
    };
    if names_asset(expanded) {
        return follow_asset(name, tag_template, page, expanded);
    }
    let source = followable(name, expanded)?;
    let tags = fetch::list_tags(&source)?;
    let Some(tag) = tag_template.newest(&tags) else {
        user_bail!("input '{name}': no tag matches {tag_template}");
    };
    let (base, fragment) = expanded
        .find('#')
        .map_or((expanded, ""), |at| expanded.split_at(at));
    let separator = if base.contains('?') { '&' } else { '?' };
    Ok(Followed {
        url: Cow::Owned(format!("{base}{separator}ref=refs/tags/{tag}{fragment}")),
        tag: Some(tag.to_owned()),
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
const TAG_SLOT: &str = "{tag}";
const VERSION_SLOT: &str = "{version}";

/// whether a fixed pin's url names its release asset with `{tag}` or
/// `{version}`
pub fn names_asset(expanded: &str) -> bool {
    expanded.contains(TAG_SLOT) || expanded.contains(VERSION_SLOT)
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

fn follow_asset(
    name: &str,
    template: &TagTemplate,
    page: Option<&TagPage>,
    expanded: &str,
) -> Result<Followed<'static>> {
    let tags = match page {
        Some(found) => found.tags(name)?,
        None => fetch::list_tags(&asset_repo(name, expanded)?)?,
    };
    let ranked = template.ranked(&tags);
    if ranked.is_empty() {
        user_bail!("input '{name}': no tag matches {template}");
    }
    for &tag in ranked.iter().take(ASSET_TRIES) {
        let version = template.asset_version(tag);
        let url = expanded
            .replace(TAG_SLOT, tag)
            .replace(VERSION_SLOT, version);
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
