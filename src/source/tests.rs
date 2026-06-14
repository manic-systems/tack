// SPDX-License-Identifier: EUPL-1.2

use std::path::Path;

use super::localize_path_url_with_warning;

#[test]
fn localize_keeps_store_copied_path_urls_reachable() {
    let tack = Path::new("/home/u/proj/.tack");

    assert_eq!(
        localize_path_url_with_warning("path:./vendor/dep", tack).url,
        "path:../vendor/dep"
    );
    assert_eq!(
        localize_path_url_with_warning("path:../sibling", tack).url,
        "path:/home/u/sibling"
    );
}
