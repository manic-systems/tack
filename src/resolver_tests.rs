use std::fs;

use super::{
    MARKER,
    PATCHED,
    PATCHED_DIFF,
    RESOLVER_NIX,
    SIGNED,
    TACK_FOLLOWS,
    TAG,
    VERSION,
    declared,
    ensure_lock,
};
use crate::{
    lock::LockFile,
    project::Project,
};

#[test]
fn bundled_resolver_declares_every_lock_feature() {
    let features = declared(RESOLVER_NIX);
    assert!(
        [PATCHED, PATCHED_DIFF, TAG, VERSION, SIGNED, TACK_FOLLOWS]
            .iter()
            .all(|feature| features.contains(feature))
    );
}

const WALKS_UPSTREAM: &str = r#"
[inputs.upstream]
url = "github:example/upstream"

[inputs.nixpkgs]
follows = "upstream/nixpkgs"
"#;

/// the bundled resolver as it was before it could walk into tack pins
fn without_tack_follows(resolver: &str) -> String {
    resolver.replacen(&format!(" {TACK_FOLLOWS}"), "", 1)
}

fn project_with(pins: &str, resolver: &str) -> (tempfile::TempDir, Project) {
    let tmp = tempfile::tempdir().unwrap();
    let project = Project::at(tmp.path().to_path_buf());
    fs::write(project.pins_path(), pins).unwrap();
    fs::write(project.resolver_path(), resolver).unwrap();
    (tmp, project)
}

#[test]
fn follows_path_refreshes_a_managed_resolver() {
    let old = without_tack_follows(RESOLVER_NIX);
    assert!(!declared(&old).contains(&TACK_FOLLOWS));
    let (_tmp, project) = project_with(WALKS_UPSTREAM, &old);
    ensure_lock(&project, &LockFile::new()).unwrap();
    assert_eq!(
        fs::read_to_string(project.resolver_path()).unwrap(),
        RESOLVER_NIX
    );
}

#[test]
fn follows_path_leaves_an_owned_resolver_alone() {
    let owned = without_tack_follows(RESOLVER_NIX).replacen(MARKER, "# my resolver.", 1);
    let (_tmp, project) = project_with(WALKS_UPSTREAM, &owned);
    ensure_lock(&project, &LockFile::new()).unwrap();
    assert_eq!(fs::read_to_string(project.resolver_path()).unwrap(), owned);
}
