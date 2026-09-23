// SPDX-License-Identifier: EUPL-1.2

use std::collections::BTreeMap;

use misstep::Result;

use crate::error::user_bail;

pub struct ShortUrls<'doc> {
    templates: BTreeMap<&'doc str, &'doc str>,
}

impl<'doc> ShortUrls<'doc> {
    pub const fn new(templates: BTreeMap<&'doc str, &'doc str>) -> Self {
        Self { templates }
    }

    pub fn expand(&self, url: &str) -> Result<String> {
        let mut expanded = url.to_owned();
        let mut chain = Vec::new();
        while let Some((scheme, rest)) = expanded.split_once(':') {
            let Some((name, template)) = self.templates.get_key_value(scheme) else {
                break;
            };
            let cycle = chain.contains(name);
            chain.push(*name);
            if cycle {
                user_bail!(
                    "shorturl cycle {} while expanding '{url}'",
                    chain.join(" -> ")
                );
            }
            expanded = template.replace("{path}", rest);
        }

        let from_alias = !chain.is_empty();
        if from_alias {
            Ok(Self::normalize_git_ref(&expanded))
        } else {
            Ok(expanded)
        }
    }

    /// nix treats the trailing segment as path depth
    fn normalize_git_ref(url: &str) -> String {
        if !url.starts_with("git+") || url.contains('?') {
            return url.to_owned();
        }
        let Some((scheme, rest)) = url.split_once("://") else {
            return url.to_owned();
        };
        let segs = rest.split('/').collect::<Vec<&str>>();
        if segs.len() < 4 {
            return url.to_owned();
        }
        let (base, reff) = segs.split_at(segs.len() - 1);
        format!("{scheme}://{}?ref={}", base.join("/"), reff[0])
    }
}
