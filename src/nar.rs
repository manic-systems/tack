// SPDX-License-Identifier: EUPL-1.2

use std::{
    fs,
    io::{
        self,
        Read as _,
    },
    os::unix::{
        ffi::OsStrExt as _,
        fs::PermissionsExt as _,
    },
    path::{
        Path,
        PathBuf,
    },
};

use data_encoding::BASE64;
use hmac_sha256::Hash as Sha256;
use misstep::{
    Result,
    ResultExt as _,
};

pub fn hash_path(root: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    emit_bytes(&mut hash, b"nix-archive-1");
    let mut path = root.to_path_buf();
    emit_node(&mut hash, &mut path)?;
    Ok(format!("sha256-{}", BASE64.encode(&hash.finalize())))
}

pub fn hash_path_and_wrapped(root: &Path, name: &str) -> Result<(String, String)> {
    let mut nar = Sha256::new();
    emit_bytes(&mut nar, b"nix-archive-1");
    let mut wrapped = Sha256::new();
    for token in [
        b"nix-archive-1".as_slice(),
        b"(",
        b"type",
        b"directory",
        b"entry",
        b"(",
        b"name",
        name.as_bytes(),
        b"node",
    ] {
        emit_bytes(&mut wrapped, token);
    }
    let mut both = [nar, wrapped];
    let mut path = root.to_path_buf();
    emit_node(&mut both, &mut path)?;
    let [nar_done, mut wrapped_open] = both;
    emit_bytes(&mut wrapped_open, b")");
    emit_bytes(&mut wrapped_open, b")");
    Ok((
        format!("sha256-{}", BASE64.encode(&nar_done.finalize())),
        format!("sha256-{}", BASE64.encode(&wrapped_open.finalize())),
    ))
}

trait Feed {
    fn feed(&mut self, bytes: &[u8]);
}

impl Feed for Sha256 {
    fn feed(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

impl Feed for [Sha256; 2] {
    fn feed(&mut self, bytes: &[u8]) {
        for hasher in self {
            hasher.update(bytes);
        }
    }
}

pub struct HashingReader<R> {
    inner:  R,
    hasher: Sha256,
}

impl<R> HashingReader<R>
where
    R: io::Read,
{
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    pub fn finish(mut self) -> io::Result<String> {
        io::copy(&mut self, &mut io::sink())?;
        Ok(format!("sha256-{}", BASE64.encode(&self.hasher.finalize())))
    }
}

impl<R> io::Read for HashingReader<R>
where
    R: io::Read,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.hasher.update(buf.get(..read).unwrap_or_default());
        Ok(read)
    }
}

#[expect(clippy::large_stack_arrays, reason = "64kb isn't that large, really")]
pub fn hash_reader<R>(mut reader: R) -> io::Result<String>
where
    R: io::Read,
{
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 0x10000];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(format!("sha256-{}", BASE64.encode(&hasher.finalize())))
}

fn emit_node<H>(hash: &mut H, path: &mut PathBuf) -> Result<()>
where
    H: Feed,
{
    let meta = fs::symlink_metadata(&path).with_context(|| format!("stat {}", path.display()))?;
    emit_bytes(hash, b"(");
    if meta.is_symlink() {
        let target = fs::read_link(&path)?;
        emit_bytes(hash, b"type");
        emit_bytes(hash, b"symlink");
        emit_bytes(hash, b"target");
        emit_bytes(hash, target.as_os_str().as_bytes());
    } else if meta.is_dir() {
        emit_bytes(hash, b"type");
        emit_bytes(hash, b"directory");
        let mut entries = fs::read_dir(&path)?
            .map(|entry| entry.map(|item| item.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        entries.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        for name in entries {
            emit_bytes(hash, b"entry");
            emit_bytes(hash, b"(");
            emit_bytes(hash, b"name");
            emit_bytes(hash, name.as_bytes());
            emit_bytes(hash, b"node");
            path.push(&name);
            emit_node(hash, path)?;
            path.pop();
            emit_bytes(hash, b")");
        }
    } else {
        emit_bytes(hash, b"type");
        emit_bytes(hash, b"regular");
        if meta.permissions().mode() & 0o111 != 0 {
            emit_bytes(hash, b"executable");
            emit_bytes(hash, b"");
        }
        emit_bytes(hash, b"contents");
        emit_contents(hash, path, meta.len())?;
    }
    emit_bytes(hash, b")");
    Ok(())
}

#[expect(clippy::large_stack_arrays, reason = "64kb isn't that large, really")]
fn emit_contents<H>(hasher: &mut H, path: &Path, len: u64) -> Result<()>
where
    H: Feed,
{
    hasher.feed(&len.to_le_bytes());
    let mut file = fs::File::open(path)?;
    let mut buf = [0_u8; 0x10000];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.feed(&buf[..read]);
        total += read as u64;
    }
    if total != len {
        misstep::bail!("{} changed size during hashing", path.display());
    }
    pad(hasher, len);
    Ok(())
}

fn emit_bytes<H>(hasher: &mut H, bytes: &[u8])
where
    H: Feed,
{
    hasher.feed(&(bytes.len() as u64).to_le_bytes());
    hasher.feed(bytes);
    pad(hasher, bytes.len() as u64);
}

fn pad<H>(hasher: &mut H, len: u64)
where
    H: Feed,
{
    let rem = (len % 8) as usize;
    if rem != 0 {
        hasher.feed(&[0_u8; 8][..8 - rem]);
    }
}
