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
use eyre::{
    Result,
    WrapErr as _,
};
use hmac_sha256::Hash as Sha256;

pub fn hash_path(root: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    emit_bytes(&mut hash, b"nix-archive-1");
    let mut path = root.to_path_buf();
    emit_node(&mut hash, &mut path)?;
    Ok(format!("sha256-{}", BASE64.encode(&hash.finalize())))
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    format!("sha256-{}", BASE64.encode(&Sha256::hash(bytes)))
}

fn emit_node(hash: &mut Sha256, path: &mut PathBuf) -> Result<()> {
    let meta = fs::symlink_metadata(&path).wrap_err_with(|| format!("stat {}", path.display()))?;
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
fn emit_contents(hasher: &mut Sha256, path: &Path, len: u64) -> Result<()> {
    hasher.update(len.to_le_bytes());
    let mut file = fs::File::open(path)?;
    let mut buf = [0_u8; 0x10000];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        total += read as u64;
    }
    if total != len {
        eyre::bail!("{} changed size during hashing", path.display());
    }
    pad(hasher, len);
    Ok(())
}

fn emit_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
    pad(hasher, bytes.len() as u64);
}

fn pad(hasher: &mut Sha256, len: u64) {
    let rem = (len % 8) as usize;
    if rem != 0 {
        hasher.update(&[0_u8; 8][..8 - rem]);
    }
}
