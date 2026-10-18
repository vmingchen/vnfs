//! Incremental, no-follow traversal shared by application adapters.
use crate::api::{DirEntry, ListDirOptions, TraversalCompletion};
use crate::{VfError, VfResult, internal::TraversalBudget};
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
    options: ListDirOptions,
    sort_by_name: bool,
    mut read_dir: impl FnMut(&Path, ListDirOptions) -> VfResult<Vec<DirEntry>>,
    mut callback: impl FnMut(&WalkEvent) -> VfResult<WalkControl>,
) -> VfResult<TraversalCompletion> {
    let mut budget = TraversalBudget::new(
        options.max_entries_or(crate::api::DEFAULT_DIRECTORY_MAX_ENTRIES),
        options.max_path_bytes_or(crate::api::DEFAULT_DIRECTORY_MAX_PATH_BYTES),
    );
    budget.charge(root.path())?;
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
    while let Some(mut event) = pending.pop() {
        match callback(&event)? {
            WalkControl::Stop => return Ok(TraversalCompletion::Stopped),
            decision if event.kind == WalkEventKind::Enter => {
                event.kind = WalkEventKind::Leave;
                if decision == WalkControl::SkipSubtree {
                    pending.push(event);
                    continue;
                }
                if event.depth > options.max_depth_or(Default::default()).get() {
                    if options.truncate_at_max_depth() {
                        pending.push(event);
                        continue;
                    }
                    return Err(VfError::client(0, libc::EFBIG as u32)
                        .with_context("walk_events", event.entry.path()));
                }
                let remaining = budget.remaining_options(ListDirOptions::new());
                let mut entries = read_dir(event.entry.path(), remaining)?;
                for entry in &entries {
                    budget
                        .charge(entry.path())
                        .map_err(|error| error.with_context("walk_events", entry.path()))?;
                }
                if sort_by_name {
                    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
                }
                let child_depth = event.depth + 1;
                pending.push(event);
                for entry in entries.into_iter().rev() {
                    let kind = if entry.attrs().is_dir() {
                        WalkEventKind::Enter
                    } else {
                        WalkEventKind::Entry
                    };
                    pending.push(WalkEvent {
                        kind,
                        entry,
                        depth: child_depth,
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
    use std::path::PathBuf;

    use super::*;
    use crate::{AttrMask, FileOperand, VfAttrs, VfType};
    fn entry(path: &str, dir: bool) -> DirEntry {
        DirEntry::new(
            path.into(),
            crate::metadata_from_attrs(VfAttrs {
                file: FileOperand::from_path(path),
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
            ListDirOptions::default().with_recursive(true),
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
    fn depth_limit_allows_boundary_entries_and_only_blocks_directory_descent() {
        let options = ListDirOptions::new()
            .with_recursive(true)
            .with_max_depth(crate::api::DepthLimit::new(0).expect("finite depth"));
        let mut events = Vec::new();
        assert_eq!(
            walk_events(
                entry("/", true),
                options,
                false,
                |path, _| {
                    assert_eq!(path, Path::new("/"));
                    Ok(vec![entry("/file", false)])
                },
                |event| {
                    events.push((event.kind, event.entry.path().to_path_buf(), event.depth));
                    Ok(WalkControl::Continue)
                },
            )
            .unwrap(),
            TraversalCompletion::Complete
        );
        assert_eq!(
            events,
            vec![
                (WalkEventKind::Enter, PathBuf::from("/"), 0),
                (WalkEventKind::Entry, PathBuf::from("/file"), 1),
                (WalkEventKind::Leave, PathBuf::from("/"), 0),
            ]
        );

        let mut strict_entries = Vec::new();
        let error = walk_events(
            entry("/", true),
            options,
            false,
            |path, _| {
                assert_eq!(path, Path::new("/"));
                Ok(vec![entry("/file", false), entry("/child", true)])
            },
            |event| {
                strict_entries.push(event.entry.path().to_path_buf());
                Ok(WalkControl::Continue)
            },
        )
        .unwrap_err();
        assert_eq!(error.raw_code(), libc::EFBIG as u32);
        assert!(strict_entries.contains(&PathBuf::from("/file")));

        let mut truncated_events = Vec::new();
        let mut listed = Vec::new();
        assert_eq!(
            walk_events(
                entry("/", true),
                options.with_truncate_at_max_depth(true),
                false,
                |path, _| {
                    listed.push(path.to_path_buf());
                    assert_eq!(path, Path::new("/"));
                    Ok(vec![entry("/file", false), entry("/child", true)])
                },
                |event| {
                    truncated_events.push((event.kind, event.entry.path().to_path_buf()));
                    Ok(WalkControl::Continue)
                },
            )
            .unwrap(),
            TraversalCompletion::Complete
        );
        assert_eq!(listed, [PathBuf::from("/")]);
        assert!(truncated_events.contains(&(WalkEventKind::Entry, PathBuf::from("/file"))));
        assert!(truncated_events.contains(&(WalkEventKind::Enter, PathBuf::from("/child"))));
    }

    #[test]
    fn stop_error_and_limits_do_not_read_descendants() {
        assert_eq!(
            walk_events(
                entry("/", true),
                ListDirOptions::default().with_recursive(true),
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
                ListDirOptions::default().with_recursive(true),
                false,
                |_, _| panic!("callback failed"),
                |_| Err(VfError::client(0, libc::EIO as u32))
            )
            .is_err()
        );
        for options in [
            ListDirOptions::new()
                .with_recursive(true)
                .with_max_entries(1),
            ListDirOptions::new()
                .with_recursive(true)
                .with_max_path_bytes(1),
            ListDirOptions::new()
                .with_recursive(true)
                .with_max_depth(crate::api::DepthLimit::new(0).expect("finite depth")),
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
                .raw_code(),
                libc::EFBIG as u32
            );
        }
    }
}
