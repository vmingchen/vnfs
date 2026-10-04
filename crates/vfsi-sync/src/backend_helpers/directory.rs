use super::*;

pub fn listdir_page_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    dir: &Path,
    masks: AttrMask,
    cursor: Option<DirPageCursor>,
    page_size: usize,
    max_entries: usize,
) -> VfResult<(Vec<VfAttrs>, Option<DirPageCursor>)> {
    if page_size == 0 {
        return Err(VfError::client(0, ERR_INVAL));
    }
    let mut remaining = match cursor {
        Some(cursor) => cursor.into_state::<std::vec::IntoIter<VfAttrs>>()?,
        None => backend
            .listdir_impl(dir, masks, max_entries, false)?
            .into_iter(),
    };
    let page: Vec<_> = remaining.by_ref().take(page_size).collect();
    let next = if remaining.len() == 0 {
        None
    } else {
        Some(DirPageCursor::new(remaining))
    };
    Ok((page, next))
}

pub fn directory_page_batch_size_default<F: Backend + ?Sized>(_backend: &F) -> usize {
    1
}

pub fn vlistdir_pages_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    dirs: &[&Path],
    masks: AttrMask,
    cursors: Vec<Option<DirPageCursor>>,
    page_size: usize,
    max_entries: usize,
) -> VfResult<Vec<BackendDirectoryPage>> {
    if dirs.len() != cursors.len() || page_size == 0 {
        return Err(VfError::client(0, ERR_INVAL));
    }
    dirs.iter()
        .zip(cursors)
        .enumerate()
        .map(|(index, (dir, cursor))| {
            backend
                .listdir_page_impl(dir, masks, cursor, page_size, max_entries)
                .map(|(entries, next)| {
                    let children = (0..entries.len()).map(|_| None).collect();
                    (entries, next, children)
                })
                .map_err(|error| error.with_index(index))
        })
        .collect()
}

pub fn walk_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    root: &Path,
    masks: AttrMask,
    sort: &mut dyn FnMut(&Path, &mut Vec<VfAttrs>),
) -> VfResult<Vec<WalkEntry>> {
    backend.walk_with_options_impl(root, masks, WalkOptions::default(), sort)
}

pub fn walk_with_options_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    root: &Path,
    masks: AttrMask,
    options: WalkOptions,
    sort: &mut dyn FnMut(&Path, &mut Vec<VfAttrs>),
) -> VfResult<Vec<WalkEntry>> {
    // Explicit stack (pre-order, subdirectories visited in the order the
    // sort callback produced) so deep trees cannot overflow the call
    // stack.
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut entry_count = 0usize;
    let mut path_bytes = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let remaining = options.entry_limit().saturating_sub(entry_count);
        let request_count = remaining.saturating_add(1);
        let mut entries = backend.listdir_impl(&dir, masks, request_count, false)?;
        if entries.len() > remaining {
            return Err(
                VfError::failure(entry_count, libc::EFBIG as u32).with_context("walk", &dir)
            );
        }
        for entry in &entries {
            let bytes = entry.file.path().map_or(0, |path| path.as_os_str().len());
            path_bytes = path_bytes.checked_add(bytes).ok_or_else(|| {
                VfError::failure(entry_count, libc::EFBIG as u32).with_context("walk", &dir)
            })?;
            if path_bytes > options.path_byte_limit() {
                return Err(
                    VfError::failure(entry_count, libc::EFBIG as u32).with_context("walk", &dir)
                );
            }
            entry_count += 1;
        }
        sort(dir.as_path(), &mut entries);
        let subdirs: Vec<PathBuf> = entries
            .iter()
            .filter(|e| e.ftype == VfType::Directory)
            .filter_map(|e| e.file.path().map(|p| p.to_path_buf()))
            .collect();
        if !subdirs.is_empty()
            && depth >= options.depth_limit()
            && !options.truncates_at_depth_limit()
        {
            return Err(
                VfError::failure(entry_count, libc::EFBIG as u32).with_context("walk", &dir)
            );
        }
        if depth < options.depth_limit() {
            for s in subdirs.into_iter().rev() {
                stack.push((s, depth + 1));
            }
        }
        out.push(WalkEntry { path: dir, entries });
    }
    Ok(out)
}

pub fn vlistdirs_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    dirs: &[&Path],
    masks: AttrMask,
    max_entries: usize,
    recursive: bool,
    cb: &mut dyn FnMut(&VfAttrs, &Path) -> bool,
) -> VfRes {
    let mut count = 0usize;
    for (i, d) in dirs.iter().enumerate() {
        if max_entries != 0 && count >= max_entries {
            break;
        }
        let remaining = if max_entries == 0 {
            0
        } else {
            max_entries - count
        };
        if !recursive {
            let mut stopped = false;
            backend
                .visit_dir_impl(d, masks, remaining, &mut |entry| {
                    count += 1;
                    if !cb(entry, d) {
                        stopped = true;
                        return false;
                    }
                    true
                })
                .map_err(|e| e.with_index(i))?;
            if stopped {
                return Ok(());
            }
            continue;
        }
        let entries = backend
            .listdir_impl(d, masks, remaining, true)
            .map_err(|e| e.with_index(i))?;
        for e in &entries {
            /* A recursive list contains descendants too, so report each
             * entry's actual parent rather than attributing every row to the
             * original root. Backend overrides follow the same contract. */
            let entry_dir = if recursive {
                e.file.path().and_then(Path::parent).unwrap_or(d)
            } else {
                d
            };
            if !cb(e, entry_dir) {
                return Ok(());
            }
            count += 1;
        }
    }
    Ok(())
}

pub fn visit_dir_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    dir: &Path,
    masks: AttrMask,
    max_entries: usize,
    cb: &mut dyn FnMut(&VfAttrs) -> bool,
) -> VfRes {
    let entries = backend.listdir_impl(dir, masks, max_entries, false)?;
    for entry in &entries {
        if !cb(entry) {
            break;
        }
    }
    Ok(())
}

pub fn native_read_dir_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
    options: ReadDirOptions,
) -> VfResult<Vec<DirEntry>> {
    let requested = options.entry_limit().saturating_add(1);
    let entries = backend
        .listdir_impl(path, metadata_mask(), requested, false)
        .map_err(|error| error.with_context("read_dir", path))?;
    if entries.len() > options.entry_limit() {
        return Err(VfError::failure(options.entry_limit(), libc::EFBIG as u32)
            .with_context("read_dir", path));
    }
    let mut path_bytes = 0usize;
    entries
        .into_iter()
        .enumerate()
        .map(|(index, attributes)| {
            let entry_path = attributes
                .file
                .path()
                .map(std::path::Path::to_path_buf)
                .ok_or_else(|| VfError::client(index, ERR_IO).with_context("read_dir", path))?;
            path_bytes = path_bytes
                .checked_add(entry_path.as_os_str().len())
                .ok_or_else(|| {
                    VfError::failure(index, libc::EFBIG as u32).with_context("read_dir", path)
                })?;
            if path_bytes > options.path_byte_limit() {
                return Err(
                    VfError::failure(index, libc::EFBIG as u32).with_context("read_dir", path)
                );
            }
            Ok(DirEntry::new(
                entry_path,
                vfsi_core::metadata_from_attrs(attributes),
            ))
        })
        .collect()
}

pub fn native_read_dir_page_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
    cursor: Option<DirPageCursor>,
    page_size: usize,
    max_entries: usize,
) -> VfResult<(Vec<DirEntry>, Option<DirPageCursor>)> {
    backend.read_dir_page_with_fields_impl(path, metadata_mask(), cursor, page_size, max_entries)
}

pub fn native_read_dir_page_with_fields_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
    fields: AttrMask,
    cursor: Option<DirPageCursor>,
    page_size: usize,
    max_entries: usize,
) -> VfResult<(Vec<DirEntry>, Option<DirPageCursor>)> {
    let (attributes, next) = backend
        .listdir_page_impl(
            path,
            fields | AttrMask::MODE,
            cursor,
            page_size,
            max_entries,
        )
        .map_err(|error| error.with_context("visit_dir", path))?;
    if attributes.len() > page_size {
        return Err(VfError::transport(
            None,
            "listdir_page_impl returned more entries than requested",
        ));
    }
    let entries = attributes
        .into_iter()
        .enumerate()
        .map(|(index, attributes)| {
            let entry_path = attributes
                .file
                .path()
                .map(std::path::Path::to_path_buf)
                .ok_or_else(|| VfError::client(index, ERR_IO).with_context("visit_dir", path))?;
            Ok(DirEntry::new(
                entry_path,
                vfsi_core::metadata_from_attrs(attributes),
            ))
        })
        .collect::<VfResult<Vec<_>>>()?;
    Ok((entries, next))
}
