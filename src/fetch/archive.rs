// SPDX-License-Identifier: EUPL-1.2

use std::{
    fs,
    io::{
        self,
        Cursor,
        Read,
    },
    path::{
        Path,
        PathBuf,
    },
    result::Result as StdResult,
};

use flate2::read::GzDecoder;
use misstep::{
    Result,
    report,
};
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
        .ok_or_else(|| report!("unknown tar format for URL: {url}"))
}

/// [`None`] for anything that isn't a tar archive, compressed or not, such as
/// an html error page
pub(super) fn sniff_tar_format(head: &[u8]) -> Option<TarFormat> {
    match *head {
        [0x1F, 0x8B, ..] => Some(TarFormat::Gz),
        [0xFD, b'7', b'z', b'X', b'Z', 0x00, ..] => Some(TarFormat::Xz),
        [0x28, 0xB5, 0x2F, 0xFD, ..] => Some(TarFormat::Zstd),
        _ => (head.get(257..262) == Some(b"ustar")).then_some(TarFormat::Plain),
    }
}

const SNIFF_LEN: u64 = 262;

pub(super) fn sniff_reader<R: Read>(mut reader: R) -> io::Result<(Option<TarFormat>, impl Read)> {
    let mut head = Vec::new();
    reader.by_ref().take(SNIFF_LEN).read_to_end(&mut head)?;
    Ok((sniff_tar_format(&head), Cursor::new(head).chain(reader)))
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
