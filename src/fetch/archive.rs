// SPDX-License-Identifier: EUPL-1.2

use std::{
    fs,
    io::Read,
    path::{
        Path,
        PathBuf,
    },
    result::Result as StdResult,
};

use eyre::{
    Result,
    eyre,
};
use flate2::read::GzDecoder;
use xz2::read::XzDecoder;

#[derive(Clone, Copy)]
pub(super) enum TarFormat {
    Gz,
    Xz,
    Plain,
}

pub(super) fn detect_tar_format(url: &str) -> Result<TarFormat> {
    let after_query = url.split('?').next().unwrap_or(url);
    let path = after_query.split('#').next().unwrap_or(after_query);
    if ends_with_ci(path, ".tar.xz") || ends_with_ci(path, ".txz") {
        Ok(TarFormat::Xz)
    } else if ends_with_ci(path, ".tar.gz") || ends_with_ci(path, ".tgz") {
        Ok(TarFormat::Gz)
    } else if ends_with_ci(path, ".tar") {
        Ok(TarFormat::Plain)
    } else {
        Err(eyre!("unknown tar format for URL: {url}"))
    }
}

/// case-insensitive ascii suffix check, bytes-based to dodge utf-8 slicing
fn ends_with_ci(path: &str, ext: &str) -> bool {
    let bytes = path.as_bytes();
    let suffix = ext.as_bytes();
    bytes.len() >= suffix.len() && bytes[bytes.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

/// unpack a tarball stream, strip the single top-level dir, return the root
pub(super) fn unpack_tar_stream<R>(reader: R, format: TarFormat, into: &Path) -> Result<PathBuf>
where
    R: Read,
{
    let boxed: Box<dyn Read> = match format {
        TarFormat::Gz => Box::new(GzDecoder::new(reader)),
        TarFormat::Xz => Box::new(XzDecoder::new(reader)),
        TarFormat::Plain => Box::new(reader),
    };
    let mut ar = tar::Archive::new(boxed);
    ar.unpack(into)?;

    let mut entries = fs::read_dir(into)?
        .filter_map(StdResult::ok)
        .collect::<Vec<_>>();
    if entries.len() == 1 && entries[0].file_type()?.is_dir() {
        Ok(entries.remove(0).path())
    } else {
        Ok(into.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        TarFormat,
        detect_tar_format,
    };

    #[test]
    fn tar_format_from_extension() {
        assert!(matches!(
            detect_tar_format("https://x/y.tar.xz").unwrap(),
            TarFormat::Xz
        ));
        assert!(matches!(
            detect_tar_format("https://x/y.txz").unwrap(),
            TarFormat::Xz
        ));
        assert!(matches!(
            detect_tar_format("https://x/y.tar.gz").unwrap(),
            TarFormat::Gz
        ));
        assert!(matches!(
            detect_tar_format("https://x/y.tgz").unwrap(),
            TarFormat::Gz
        ));
        assert!(matches!(
            detect_tar_format("https://x/y.tar").unwrap(),
            TarFormat::Plain
        ));
        // querystring and fragment must not defeat detection
        assert!(matches!(
            detect_tar_format("https://x/y.tar.xz?signed=1#frag").unwrap(),
            TarFormat::Xz
        ));
        assert!(detect_tar_format("https://x/y").is_err());
    }
}
