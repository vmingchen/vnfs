//! Incremental, no-follow traversal shared by application adapters.
use crate::api::{DirEntry, ReadDirOptions, TraversalCompletion, WalkOptions};
use crate::{VfError, VfResult};
use std::path::Path;

/// Callback decision. Pruning never reads the directory's contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkControl {
    Continue,
    SkipSubtree,
    Stop,
}
/// Enter/Leave surround a directory, including a pruned directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkEventKind {
    Enter,
    Entry,
    Leave,
}
/// Root is depth zero. Symlinks are Entry events and are never followed.
#[derive(Debug, Clone)]
pub struct WalkEvent {
    pub kind: WalkEventKind,
    pub entry: DirEntry,
    pub depth: usize,
}

/// Adapter hook; application callers should use their client's method.
/// Each directory is bounded before sorting; only the active frontier is kept.
/// Limits charge fetched entries (including the root), even if later pruned.
#[doc(hidden)]
pub fn walk_events(
    root: DirEntry,
    options: WalkOptions,
    sort_by_name: bool,
    mut read_dir: impl FnMut(&Path, ReadDirOptions) -> VfResult<Vec<DirEntry>>,
    mut callback: impl FnMut(&WalkEvent) -> VfResult<WalkControl>,
) -> VfResult<TraversalCompletion> {
    let mut count = 1usize;
    let mut bytes = root.path().as_os_str().len();
    if count > options.entry_limit() || bytes > options.path_byte_limit() {
        return Err(VfError::client(0, libc::EFBIG as u32));
    }
    let kind = if root.attrs().is_dir() {
        WalkEventKind::Enter
    } else {
        WalkEventKind::Entry
    };
    let mut pending = vec![WalkEvent {
        kind,
        entry: root,
        depth: 0,
    }];
    while let Some(event) = pending.pop() {
        match callback(&event)? {
            WalkControl::Stop => return Ok(TraversalCompletion::Stopped),
            decision if event.kind == WalkEventKind::Enter => {
                pending.push(WalkEvent {
                    kind: WalkEventKind::Leave,
                    ..event.clone()
                });
                if decision == WalkControl::SkipSubtree {
                    continue;
                }
                if event.depth >= options.depth_limit() && options.truncates_at_depth_limit() {
                    continue;
                }
                let remaining = ReadDirOptions::new()
                    .max_entries(options.entry_limit().saturating_sub(count))
                    .max_path_bytes(options.path_byte_limit().saturating_sub(bytes));
                let mut entries = read_dir(event.entry.path(), remaining)?;
                for entry in &entries {
                    count = count
                        .checked_add(1)
                        .ok_or_else(|| VfError::client(0, libc::EFBIG as u32))?;
                    bytes = bytes
                        .checked_add(entry.path().as_os_str().len())
                        .ok_or_else(|| VfError::client(0, libc::EFBIG as u32))?;
                    if count > options.entry_limit()
                        || bytes > options.path_byte_limit()
                        || event.depth >= options.depth_limit()
                    {
                        return Err(VfError::client(0, libc::EFBIG as u32)
                            .with_context("walk_events", entry.path()));
                    }
                }
                if sort_by_name {
                    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
                }
                for entry in entries.into_iter().rev() {
                    let kind = if entry.attrs().is_dir() {
                        WalkEventKind::Enter
                    } else {
                        WalkEventKind::Entry
                    };
                    pending.push(WalkEvent {
                        kind,
                        entry,
                        depth: event.depth + 1,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(TraversalCompletion::Complete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttrMask, VfAttrs, VfFile, VfType};
    fn entry(path: &str, dir: bool) -> DirEntry {
        DirEntry::new(
            path.into(),
            crate::metadata_from_attrs(VfAttrs {
                file: VfFile::from_path(path),
                masks: AttrMask::MODE,
                ftype: if dir {
                    VfType::Directory
                } else {
                    VfType::Regular
                },
                ..Default::default()
            }),
        )
    }
    #[test]
    fn prune_prevents_io_and_events_have_depth_first_order() {
        let mut events = Vec::new();
        walk_events(
            entry("/", true),
            WalkOptions::default(),
            true,
            |path, _| {
                assert_eq!(path, Path::new("/"), "pruned directory must not be read");
                Ok(vec![entry("/z", false), entry("/a", true)])
            },
            |event| {
                events.push((event.kind, event.entry.path().to_path_buf(), event.depth));
                Ok(if event.entry.path() == Path::new("/a") {
                    WalkControl::SkipSubtree
                } else {
                    WalkControl::Continue
                })
            },
        )
        .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|e| (e.0, e.1.as_path(), e.2))
                .collect::<Vec<_>>(),
            vec![
                (WalkEventKind::Enter, Path::new("/"), 0),
                (WalkEventKind::Enter, Path::new("/a"), 1),
                (WalkEventKind::Leave, Path::new("/a"), 1),
                (WalkEventKind::Entry, Path::new("/z"), 1),
                (WalkEventKind::Leave, Path::new("/"), 0)
            ]
        );
    }
    #[test]
    fn stop_error_and_limits_do_not_read_descendants() {
        assert_eq!(
            walk_events(
                entry("/", true),
                WalkOptions::default(),
                false,
                |_, _| panic!("stopped"),
                |_| Ok(WalkControl::Stop)
            )
            .unwrap(),
            TraversalCompletion::Stopped
        );
        assert!(
            walk_events(
                entry("/", true),
                WalkOptions::default(),
                false,
                |_, _| panic!("callback failed"),
                |_| Err(VfError::client(0, libc::EIO as u32))
            )
            .is_err()
        );
        for options in [
            WalkOptions::new().max_entries(1),
            WalkOptions::new().max_path_bytes(1),
            WalkOptions::new().max_depth(0),
        ] {
            assert_eq!(
                walk_events(
                    entry("/", true),
                    options,
                    false,
                    |path, _| {
                        assert_eq!(path, Path::new("/"));
                        Ok(vec![entry("/a", true)])
                    },
                    |_| Ok(WalkControl::Continue)
                )
                .unwrap_err()
                .err_no(),
                libc::EFBIG as u32
            );
        }
    }
}
