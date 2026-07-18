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
use zstd::stream::read::Decoder as ZstdDecoder;

#[derive(Clone, Copy)]
pub(super) enum TarFormat {
    Gz,
    Xz,
    Zstd,
    Plain,
}

const TAR_FORMAT_SUFFIXES: [(&str, TarFormat); 7] = [
    (".tar.gz", TarFormat::Gz),
    (".tgz", TarFormat::Gz),
    (".tar.xz", TarFormat::Xz),
    (".txz", TarFormat::Xz),
    (".tar.zst", TarFormat::Zstd),
    (".tzst", TarFormat::Zstd),
    (".tar", TarFormat::Plain),
];

pub(super) fn detect_tar_format(url: &str) -> Result<TarFormat> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    TAR_FORMAT_SUFFIXES
        .into_iter()
        .find_map(|(suffix, format)| ends_with_ci(path, suffix).then_some(format))
        .ok_or_else(|| eyre!("unknown tar format for URL: {url}"))
}

#[inline]
fn ends_with_ci(path: &str, ext: &str) -> bool {
    let bytes = path.as_bytes();
    let suffix = ext.as_bytes();
    bytes.len() >= suffix.len() && bytes[bytes.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

pub(super) fn unpack_tar_stream<R>(reader: R, format: TarFormat, into: &Path) -> Result<PathBuf>
where
    R: Read,
{
    let boxed: Box<dyn Read> = match format {
        TarFormat::Gz => Box::new(GzDecoder::new(reader)),
        TarFormat::Xz => Box::new(XzDecoder::new(reader)),
        TarFormat::Zstd => Box::new(ZstdDecoder::new(reader)?),
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
