// SPDX-License-Identifier: EUPL-1.2

use std::collections::BTreeMap;

pub struct ShortUrls<'a> {
    templates: BTreeMap<&'a str, &'a str>,
}

impl<'a> ShortUrls<'a> {
    pub const fn new(templates: BTreeMap<&'a str, &'a str>) -> Self {
        Self { templates }
    }

    pub fn expand(&self, url: &str) -> String {
        let Some((scheme, rest)) = url.split_once(':') else {
            return url.to_owned();
        };
        let Some(template) = self.templates.get(scheme) else {
            return url.to_owned();
        };
        Self::normalize_git_ref(&template.replace("{path}", rest))
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
