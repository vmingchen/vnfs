//! Execution policies for the scalar directory visitor; all I/O uses Vfsi vectors.
use super::{
    AttrsOptions, ControlFlow, DirEntry, Error, ListDirOptions, Result, TraversalCompletion, Vfsi,
    VfsiExt, WalkControl, WalkEvent, WalkEventKind,
};
use std::path::Path;

pub(super) fn listdir<F: Vfsi + ?Sized>(
    fs: &F,
    root: &Path,
    options: ListDirOptions,
    mut visitor: impl FnMut(&WalkEvent) -> Result<WalkControl>,
) -> Result<TraversalCompletion> {
    if options.emits_enter_leave() || options.sorts_by_name() {
        return buffered(fs, root, options, visitor);
    }

    // Keep the native paging engine, including anchored child cursors, linear
    // deep-path traversal, sibling batching, recovery and aggregate budgets.
    let mut stopped = false;
    let completions = fs
        .vlistdirs(&[root], options, |index, page| {
            if index != 0 {
                return Err(Error::transport(None, "invalid listdir page index"));
            }
            let relative = page
                .path
                .strip_prefix(root)
                .map_err(|_| Error::transport(None, "invalid listdir page parent"))?;
            if (!options.is_recursive() && page.path != root)
                || relative
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
            {
                return Err(Error::transport(None, "invalid listdir page parent"));
            }
            let depth = relative.components().count() + 1;
            for entry in page.entries {
                let event = WalkEvent {
                    kind: WalkEventKind::Entry,
                    entry,
                    depth,
                };
                match visitor(&event)? {
                    WalkControl::Stop => {
                        stopped = true;
                        return Ok(ControlFlow::Break(()));
                    }
                    WalkControl::SkipSubtree
                        if options.is_recursive() && event.entry.attrs().is_dir() =>
                    {
                        return Err(pruning_error(event.entry.path()));
                    }
                    _ => {}
                }
            }
            Ok(ControlFlow::Continue(()))
        })
        .map_err(|error| {
            if error.index().is_some_and(|index| index != 0) {
                Error::transport(None, "invalid listdir backend error index")
            } else {
                error
            }
        })?;
    let expected = if stopped {
        TraversalCompletion::Stopped
    } else {
        TraversalCompletion::Complete
    };
    if completions.as_slice() != [expected] {
        return Err(Error::transport(None, "invalid listdir completion count"));
    }
    Ok(expected)
}

fn pruning_error(path: &Path) -> Error {
    Error::client(0, libc::EINVAL as u32)
        .with_context("listdir: set enter_leave(true) for SkipSubtree", path)
}

fn limit_error(path: &Path) -> Error {
    Error::client(0, libc::EFBIG as u32).with_context("listdir", path)
}

fn buffered<F: Vfsi + ?Sized>(
    fs: &F,
    root: &Path,
    options: ListDirOptions,
    mut visitor: impl FnMut(&WalkEvent) -> Result<WalkControl>,
) -> Result<TraversalCompletion> {
    let fields = options.attributes();
    let lifecycle = options.emits_enter_leave();
    let attrs = if lifecycle || options.is_recursive() {
        fs.attrs_with_options(
            root,
            AttrsOptions::new().fields(fields).follow_symlinks(false),
        )?
    } else {
        // The entry-only root is never exposed. READDIR validates its type;
        // avoid an unnecessary stat and a no-follow requirement on SMB.
        crate::metadata_from_attrs(crate::VfAttrs {
            file: crate::VfFile::from_os_path(root),
            masks: crate::AttrMask::MODE,
            ftype: crate::VfType::Directory,
            ..Default::default()
        })
    };
    if !lifecycle && !attrs.is_dir() {
        return Err(Error::client(0, libc::ENOTDIR as u32).with_context("listdir", root));
    }
    let walk = options.walk_options(fs.limits());
    // The shared event walker charges the root. Entry-only listing excludes it
    // from the entry budget; shallow listing also excludes its retained path.
    let mut budget = walk;
    if !lifecycle {
        budget = budget.max_entries(walk.entry_limit().saturating_add(1));
        if !options.is_recursive() {
            budget = budget.max_path_bytes(
                walk.path_byte_limit()
                    .saturating_add(root.as_os_str().len()),
            );
        }
    }
    // ListDirOptions depth limits concern descent, not files in a root listing.
    // Admission below enforces them after allowing a directory to be pruned.
    budget = budget.max_depth(usize::MAX).truncate_at_max_depth(false);
    let mut retained_bytes = 0usize;
    super::traversal::walk_events(
        DirEntry::new(root.to_path_buf(), attrs),
        budget,
        options.sorts_by_name(),
        |path, remaining| {
            // Match native recursive entry-mode accounting: child paths are
            // charged once as entries and again when retained for descent.
            if !lifecycle && options.is_recursive() && path != root {
                retained_bytes = retained_bytes
                    .checked_add(path.as_os_str().len())
                    .ok_or_else(|| limit_error(path))?;
            }
            let bytes = remaining
                .path_byte_limit()
                .checked_sub(retained_bytes)
                .ok_or_else(|| limit_error(path))?;
            fs.read_dir_with_options(
                path,
                ListDirOptions::from(remaining.max_path_bytes(bytes))
                    .fields(fields)
                    // Reopening a path is not anchored to the original page.
                    // Require atomic no-follow resolution, including ancestors.
                    .follow_symlinks(
                        options.follows_symlinks() && !options.is_recursive() && !lifecycle,
                    ),
            )
        },
        |event| {
            if event.kind == WalkEventKind::Leave {
                return if lifecycle && (options.is_recursive() || event.depth == 0) {
                    visitor(event)
                } else {
                    Ok(WalkControl::Continue)
                };
            }
            if event.depth == 0 && event.kind == WalkEventKind::Enter {
                return if lifecycle {
                    visitor(event)
                } else {
                    Ok(WalkControl::Continue)
                };
            }
            let decision =
                if (lifecycle && options.is_recursive()) || event.kind == WalkEventKind::Entry {
                    visitor(event)?
                } else {
                    visitor(&WalkEvent {
                        kind: WalkEventKind::Entry,
                        ..event.clone()
                    })?
                };
            if decision == WalkControl::Stop {
                return Ok(decision);
            }
            if event.kind == WalkEventKind::Enter {
                if options.is_recursive() && !lifecycle && decision == WalkControl::SkipSubtree {
                    return Err(pruning_error(event.entry.path()));
                }
                if !options.is_recursive() || decision == WalkControl::SkipSubtree {
                    return Ok(WalkControl::SkipSubtree);
                }
                if event.depth > walk.depth_limit() {
                    if !walk.truncates_at_depth_limit() {
                        return Err(limit_error(event.entry.path()));
                    }
                    return Ok(WalkControl::SkipSubtree);
                }
            }
            Ok(decision)
        },
    )
}
