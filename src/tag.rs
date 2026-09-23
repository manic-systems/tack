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

use misstep::Result;

use crate::{
    error::user_bail,
    fetch,
    source::Source,
};

/// a tag name with one `{version}` slot, which only matches integers joined
/// by one kind of separator, `.`, `-` or `_`, so `v{version}` skips
/// `v2.0.3-purple`, `v2.1-rc1` and `v2.1.0-1`
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
        tags.iter()
            .filter_map(|tag| Some((self.version(tag)?, tag)))
            .max()
            .map(|(_, tag)| tag.as_str())
    }

    fn version(&self, tag: &str) -> Option<Vec<u64>> {
        let middle = tag
            .strip_prefix(self.prefix.as_str())?
            .strip_suffix(self.suffix.as_str())?;
        let separator = middle.chars().find(|ch| matches!(ch, '.' | '-' | '_'));
        middle
            .split(|ch| Some(ch) == separator)
            .map(|part| {
                let numeric = !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
                numeric.then(|| part.parse::<u64>().ok()).flatten()
            })
            .collect()
    }
}

pub struct Followed<'url> {
    pub url: Cow<'url, str>,
    pub tag: Option<String>,
}

/// rewrites `expanded` to the newest tag `template` matches, or passes it
/// through untouched for a pin with no template
pub fn follow<'url>(
    name: &str,
    template: Option<&TagTemplate>,
    expanded: &'url str,
) -> Result<Followed<'url>> {
    let Some(tag_template) = template else {
        return Ok(Followed {
            url: Cow::Borrowed(expanded),
            tag: None,
        });
    };
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
