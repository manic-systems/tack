use super::{
    PATCHED,
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
        [PATCHED, TAG, VERSION, SIGNED]
            .iter()
            .all(|feature| features.contains(feature))
    );
}
