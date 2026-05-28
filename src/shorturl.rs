// SPDX-License-Identifier: EUPL-1.2

use std::collections::BTreeMap;

/// expand scheme:rest via the {path} template; non-shorturl urls pass through
pub fn expand(url: &str, shorturls: &BTreeMap<&str, &str>) -> String {
    let Some((scheme, rest)) = url.split_once(':') else {
        return url.to_owned();
    };
    let Some(template) = shorturls.get(scheme) else {
        return url.to_owned();
    };
    normalize_git_ref(&template.replace("{path}", rest))
}

/// nix reads git+host/owner/repo/branch as a deeper path not a ref; remap the
/// trailing segment to ?ref=
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

#[cfg(test)]
mod tests {
    use super::*;

    fn urls() -> BTreeMap<&'static str, &'static str> {
        BTreeMap::from([
            ("atagen", "git+https://git.lobotomise.me/atagen/{path}"),
            ("amaan", "github:amaanq/{path}"),
            ("wry", "git+ssh://forgejo@git.wry.land/{path}"),
        ])
    }

    #[test]
    fn passthrough_and_github() {
        let urls = urls();
        assert_eq!(
            expand("github:NixOS/nixpkgs/nixos-unstable", &urls),
            "github:NixOS/nixpkgs/nixos-unstable"
        );
        assert_eq!(
            expand("amaan:helium-flake", &urls),
            "github:amaanq/helium-flake"
        );
    }

    #[test]
    fn git_triple_slash_remapped() {
        let urls = urls();
        assert_eq!(
            expand("atagen:meat", &urls),
            "git+https://git.lobotomise.me/atagen/meat"
        );
        assert_eq!(
            expand("wry:entailz/toes", &urls),
            "git+ssh://forgejo@git.wry.land/entailz/toes"
        );
        assert_eq!(
            expand("atagen:proj/branch", &urls),
            "git+https://git.lobotomise.me/atagen/proj?ref=branch"
        );
        assert_eq!(
            expand("wry:owner/repo/branch", &urls),
            "git+ssh://forgejo@git.wry.land/owner/repo?ref=branch"
        );
    }

    #[test]
    fn existing_query_preserved() {
        let urls = urls();
        assert_eq!(
            expand("wry:wry/wry?ref=anims-multiphase", &urls),
            "git+ssh://forgejo@git.wry.land/wry/wry?ref=anims-multiphase"
        );
    }
}
