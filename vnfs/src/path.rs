//! Byte-preserving path helpers for Unix-native `Path`/`OsStr` types.
//!
//! Unix filenames are byte strings. `PathBuf`/`OsStr` store them losslessly,
//! but NFS component operations and caches are easier with byte slices.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

/// View a native Unix path as its raw bytes.
#[cfg(unix)]
pub fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// Reconstruct a native Unix path from raw bytes.
#[cfg(unix)]
pub fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

/// Split a slash-delimited byte path into `(parent, final component)`.
pub fn split_path_bytes(path: &[u8]) -> Result<(Vec<u8>, Vec<u8>), ()> {
    let trimmed = trim_slashes(path);
    if trimmed.is_empty() {
        return Err(());
    }
    let mut parts: Vec<&[u8]> = trimmed.split(|&b| b == b'/').collect();
    let name = parts.pop().unwrap_or_default().to_vec();
    while parts.last().is_some_and(|p| p.is_empty()) {
        parts.pop();
    }
    let mut dir = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            dir.push(b'/');
        }
        dir.extend_from_slice(part);
    }
    Ok((dir, name))
}

/// Join a directory path and a component name.
pub fn join_path_bytes(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        return name.to_vec();
    }
    let mut out = dir.to_vec();
    if out.last() != Some(&b'/') {
        out.push(b'/');
    }
    out.extend_from_slice(name);
    out
}

/// Lexically normalize a root-relative byte path.
pub fn normalize_bytes(path: &[u8]) -> Vec<u8> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    for comp in trim_slashes(path).split(|&b| b == b'/') {
        match comp {
            [] | b"." => {}
            b".." => {
                parts.pop();
            }
            c => parts.push(c.to_vec()),
        }
    }
    let mut out = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(part);
    }
    out
}

/// Split a path into non-empty byte components.
pub fn components_bytes(path: &[u8]) -> Vec<Vec<u8>> {
    trim_slashes(path)
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Build a C string from raw bytes; NUL is the only rejected byte.
pub fn cstring_from_bytes(bytes: &[u8]) -> Option<std::ffi::CString> {
    std::ffi::CString::new(bytes).ok()
}

fn trim_slashes(path: &[u8]) -> &[u8] {
    let start = path.iter().position(|&b| b != b'/').unwrap_or(path.len());
    let end = path
        .iter()
        .rposition(|&b| b != b'/')
        .map(|i| i + 1)
        .unwrap_or(start);
    &path[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_utf8_components_roundtrip() {
        let (dir, name) = split_path_bytes(b"/a/b\xff/c").unwrap();
        assert_eq!(dir, b"a/b\xff");
        assert_eq!(name, b"c");
        assert_eq!(join_path_bytes(&dir, &name), b"a/b\xff/c");
        assert_eq!(normalize_bytes(b"a/./b/../c\xff"), b"a/c\xff");
    }

    #[test]
    fn native_path_roundtrip() {
        let p = path_from_bytes(b"a\xffb/c");
        assert_eq!(path_bytes(&p), b"a\xffb/c");
    }
}
