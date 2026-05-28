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

use anyhow::{
    Context as _,
    Result,
};
use sha2::{
    Digest as _,
    Sha256,
};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// sri nar hash of `root`, matching builtins.fetchTree
pub fn hash_path(root: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    emit_bytes(&mut hash, b"nix-archive-1");
    let mut path = root.to_path_buf();
    emit_node(&mut hash, &mut path)?;
    Ok(format!("sha256-{}", b64(&hash.finalize())))
}

fn emit_node(hash: &mut Sha256, path: &mut PathBuf) -> Result<()> {
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
        anyhow::bail!("{} changed size during hashing", path.display());
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

fn b64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let triple = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let packed = (u32::from(triple[0]) << 16_u32)
            | (u32::from(triple[1]) << 8_u32)
            | u32::from(triple[2]);
        out.push(B64[((packed >> 18_u32) & 63_u32) as usize] as char);
        out.push(B64[((packed >> 12_u32) & 63_u32) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[((packed >> 6_u32) & 63_u32) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(packed & 63_u32) as usize] as char
        } else {
            '='
        });
    }
    out
}
