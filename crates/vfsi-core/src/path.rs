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
#[cfg(unix)]
#[doc(hidden)]
#[allow(clippy::result_unit_err)]
pub fn split_path_bytes(path: &[u8]) -> Result<(Vec<u8>, Vec<u8>), ()> {
    let trimmed = trim_slashes(path);
    if trimmed.is_empty() {
        return Err(());
    }
    // Only the two returned buffers need ownership. Preserve interior
    // separators while dropping those immediately before the final component.
    let (dir, name) = match trimmed.iter().rposition(|&b| b == b'/') {
        Some(split) => (&trimmed[..split], &trimmed[split + 1..]),
        None => (&b""[..], trimmed),
    };
    Ok((trim_slashes(dir).to_vec(), name.to_vec()))
}

/// Join a directory path and a component name.
#[cfg(unix)]
pub fn join_path_bytes(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        return name.to_vec();
    }
    let separator = dir.last() != Some(&b'/');
    let mut out = Vec::with_capacity(dir.len() + usize::from(separator) + name.len());
    out.extend_from_slice(dir);
    if separator {
        out.push(b'/');
    }
    out.extend_from_slice(name);
    out
}

/// Lexically normalize a root-relative byte path.
pub fn normalize_bytes(path: &[u8]) -> Vec<u8> {
    // Components only live for this call; borrow them from the input.
    let mut parts: Vec<&[u8]> = Vec::new();
    for comp in trim_slashes(path).split(|&b| b == b'/') {
        match comp {
            [] | b"." => {}
            b".." => {
                parts.pop();
            }
            c => parts.push(c),
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
#[cfg(unix)]
pub fn components_bytes(path: &[u8]) -> Vec<Vec<u8>> {
    trim_slashes(path)
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Build a C string from raw bytes; NUL is the only rejected byte.
#[cfg(unix)]
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
    fn fixed_bytes_normalization_preserves_component_and_parent_rules() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"", b""),
            (b"////", b""),
            (b"./../..", b""),
            (b"../../a", b"a"),
            (b"/a//b/./../c///", b"a/c"),
            (b"a/b/../../c/..", b""),
            (b"a/\xff/../\xfe", b"a/\xfe"),
            (b"a/\x00/b", b"a/\x00/b"),
        ];
        for &(input, expected) in cases {
            assert_eq!(normalize_bytes(input), expected, "input={input:?}");
        }
    }

    #[test]
    fn allocation_path_split_preserves_separators_and_arbitrary_bytes() {
        let cases: &[(&[u8], &[u8], &[u8])] = &[
            (b"leaf", b"", b"leaf"),
            (b"///leaf///", b"", b"leaf"),
            (b"/a///b", b"a", b"b"),
            (b"a//b///c/", b"a//b", b"c"),
            (b"a/./../b", b"a/./..", b"b"),
            (b"a/\xff//\x00/\xfe", b"a/\xff//\x00", b"\xfe"),
        ];
        for &(input, expected_dir, expected_name) in cases {
            let (dir, name) = split_path_bytes(input).unwrap();
            assert_eq!(dir, expected_dir, "input={input:?}");
            assert_eq!(name, expected_name, "input={input:?}");
        }
        assert!(split_path_bytes(b"").is_err());
        assert!(split_path_bytes(b"////").is_err());
        for &(dir, name, expected) in &[
            (&b""[..], &b"leaf"[..], &b"leaf"[..]),
            (&b"a"[..], &b""[..], &b"a/"[..]),
            (&b"a//"[..], &b"\xff"[..], &b"a//\xff"[..]),
            (&b"a\x00"[..], &b"\xfe"[..], &b"a\x00/\xfe"[..]),
        ] {
            assert_eq!(join_path_bytes(dir, name), expected);
        }
    }

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
