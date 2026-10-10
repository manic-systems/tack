use super::{
    PATCHED,
    PATCHED_DIFF,
    RESOLVER_NIX,
    SIGNED,
    TAG,
    VERSION,
    declared,
};

#[test]
fn bundled_resolver_declares_every_lock_feature() {
    let features = declared(RESOLVER_NIX);
    assert!(
        [PATCHED, PATCHED_DIFF, TAG, VERSION, SIGNED]
            .iter()
            .all(|feature| features.contains(feature))
    );
}
