//! Shared algorithms over native backend contracts.

use crate::*;

use crate::native::{bytes_to_path, metadata_mask, system_time_parts, translate_open_flags};

use crate::traits::{
    take_single_result, validate_read_into_results, validate_read_results, validate_write_results,
};

use vfsi_core::internal::ManyResults;

use std::path::{Path, PathBuf};

pub fn vstatfs_impl_default<F: FileSystem + ?Sized>(
    _backend: &mut F,
    files: &[VfFile],
) -> VfResult<Vec<FilesystemStats>> {
    if files.is_empty() {
        Ok(Vec::new())
    } else {
        Err(VfError::unsupported(0))
    }
}

pub fn close_deferred_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    file: &VfFile,
) -> VfResult<()> {
    backend.close_impl(file)
}

pub fn take_notifications_default<F: FileSystem + ?Sized>(
    _backend: &mut F,
) -> Vec<Box<dyn FnOnce() + Send>> {
    Vec::new()
}

pub fn capability_bits_default<F: FileSystem + ?Sized>(_backend: &F) -> u64 {
    0
}

pub fn typed_capabilities_default<F: FileSystem + ?Sized>(backend: &F) -> Capabilities {
    Capabilities::from_bits_retain(backend.capability_bits())
}

pub fn abs_path_default<F: FileSystem + ?Sized>(_backend: &F, path: &Path) -> PathBuf {
    path.to_path_buf()
}

pub fn open_path_impl_default<F: FileSystem + ?Sized>(
    _backend: &mut F,
    base: VfPathBase,
    pathname: &Path,
    flags: i32,
    mode: u32,
) -> VfResult<VfFile> {
    let _ = (base, pathname, flags, mode);
    Err(VfError::unsupported(0))
}

pub fn sync_all_default<F: FileSystem + ?Sized>(backend: &mut F, tcf: &VfFile) -> VfResult<()> {
    backend.sync_data(tcf)
}

pub fn chdir_default<F: FileSystem + ?Sized>(_backend: &mut F, path: &Path) -> VfResult<()> {
    let _ = path;
    Err(VfError::unsupported(0))
}

pub fn getcwd_default<F: FileSystem + ?Sized>(_backend: &F) -> PathBuf {
    PathBuf::from("/")
}

pub fn seek_raw_impl_default<F: FileSystem + ?Sized>(
    _backend: &mut F,
    tcf: &VfFile,
    offset: i64,
    whence: SeekFrom,
) -> VfResult<i64> {
    let _ = (tcf, offset, whence);
    Err(VfError::unsupported(0))
}

pub fn vf_path_default<F: FileSystem + ?Sized>(backend: &F, file: &VfFile) -> VfResult<PathBuf> {
    match file {
        VfFile::Path {
            base: VfPathBase::Abs,
            path,
        } => Ok(backend.abs_path(&Path::new("/").join(path))),
        VfFile::Path {
            base: VfPathBase::Cwd,
            path,
        }
        | VfFile::CwdPath(path) => Ok(backend.abs_path(path)),
        VfFile::Cwd => Ok(backend.abs_path(Path::new(""))),
        VfFile::Descriptor(_) | VfFile::Saved => Err(VfError::failure(0, ERR_INVAL)),
        _ => Err(VfError::failure(0, ERR_INVAL)),
    }
}

pub fn open_raw_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    pathname: &Path,
    flags: i32,
    mode: u32,
) -> VfResult<VfFile> {
    backend.open_path_impl(VfPathBase::Cwd, pathname, flags, mode)
}

pub fn read_raw_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    file: &VfFile,
    offset: u64,
    length: usize,
) -> VfResult<Vec<u8>> {
    let request = ReadOp::at(file.clone(), offset, length);
    let result = backend.read_impl(&request)?;
    validate_read_results(
        "read_raw",
        std::slice::from_ref(&request),
        std::slice::from_ref(&result),
    )?;
    Ok(result.data)
}

pub fn write_raw_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    file: &VfFile,
    offset: u64,
    data: &[u8],
) -> VfResult<usize> {
    let request = WriteOpRef {
        file,
        offset: VfOffset::At(offset),
        data,
        creation: false,
        truncate: false,
    };
    let result = backend.write_impl(request)?;
    validate_write_results(
        "write_raw",
        std::slice::from_ref(&request),
        std::slice::from_ref(&result),
    )?;
    Ok(result.written)
}

pub fn vread_into_impl_default<F: VectorFileSystem + ?Sized>(
    backend: &mut F,
    reads: &[ReadOp],
    buffers: &mut [&mut [u8]],
) -> VfResult<Vec<ReadIntoResult>> {
    if reads.len() != buffers.len() {
        return Err(VfError::client(0, ERR_INVAL));
    }
    for (index, (request, buffer)) in reads.iter().zip(buffers.iter()).enumerate() {
        if request.length != buffer.len() {
            return Err(VfError::client(index, ERR_INVAL));
        }
    }
    let results = backend.vread_impl(reads)?;
    validate_read_results("vread_into_impl", reads, &results)?;
    Ok(results
        .into_iter()
        .zip(buffers.iter_mut())
        .map(|(result, buffer)| {
            buffer[..result.data.len()].copy_from_slice(&result.data);
            ReadIntoResult {
                file: result.file,
                offset: result.offset,
                read: result.data.len(),
                eof: result.eof,
            }
        })
        .collect())
}

pub fn vwrite_impl_default<F: VectorFileSystem + ?Sized>(
    backend: &mut F,
    writes: &[WriteOpRef<'_>],
) -> VfResult<Vec<WriteResult>> {
    let owned: Vec<WriteOp> = writes
        .iter()
        .map(|write| WriteOp {
            file: write.file.clone(),
            offset: write.offset,
            data: write.data.to_vec(),
            creation: write.creation,
            truncate: write.truncate,
        })
        .collect();
    backend.vwrite_owned_impl(&owned)
}

pub fn vopen_outcomes_impl_default<F: VectorFileSystem + ?Sized>(
    backend: &mut F,
    paths: &[&Path],
    flags: &[i32],
    modes: &[u32],
) -> VfResult<ManyResults<VfFile>> {
    if paths.len() != flags.len() || paths.len() != modes.len() {
        return Err(VfError::failure(0, ERR_INVAL));
    }
    let mut results = Vec::with_capacity(paths.len());
    for ((path, flag), mode) in paths.iter().zip(flags).zip(modes) {
        match backend.open_raw_impl(path, *flag, *mode) {
            Ok(file) => results.push(Ok(file)),
            Err(error) => {
                results.push(Err(error));
                break;
            }
        }
    }
    Ok(ManyResults::new(paths.len(), results))
}

pub fn before_open_cleanup_default<F: VectorFileSystem + ?Sized>(
    _backend: &mut F,
    _index: usize,
    _file: &VfFile,
) -> VfResult<()> {
    Ok(())
}

pub fn vopen_raw_impl_default<F: VectorFileSystem + ?Sized>(
    backend: &mut F,
    paths: &[&Path],
    flags: &[i32],
    modes: &[u32],
) -> VfResult<Vec<VfFile>> {
    let results = backend.vopen_outcomes_impl(paths, flags, modes)?;
    results.try_collect_with_cleanup(paths.len(), |index, file| {
        let injected = backend.before_open_cleanup(index, file);
        let closed = backend.close_impl(file);
        injected.and(closed)
    })
}

pub fn vopen_raw_simple_impl_default<F: VectorFileSystem + ?Sized>(
    backend: &mut F,
    paths: &[&Path],
    flags: i32,
    mode: u32,
) -> VfResult<Vec<VfFile>> {
    let flags_v = vec![flags; paths.len()];
    let modes_v = vec![mode; paths.len()];
    backend.vopen_raw_impl(paths, &flags_v, &modes_v)
}

pub fn vclose_impl_default<F: VectorFileSystem + ?Sized>(
    backend: &mut F,
    files: &[VfFile],
) -> VfRes {
    for (i, f) in files.iter().enumerate() {
        backend.close_impl(f).map_err(|e| e.with_index(i))?;
    }
    Ok(())
}

pub fn stat_impl_default<F: MetadataFileSystem + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<VfAttrs> {
    let mut a = VfAttrs {
        file: VfFile::from_os_path(path),
        masks: AttrMask::stat(),
        ..VfAttrs::default()
    };
    backend.vgetattrs_impl(std::slice::from_mut(&mut a))?;
    Ok(a)
}

pub fn lstat_impl_default<F: MetadataFileSystem + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<VfAttrs> {
    let mut a = VfAttrs {
        file: VfFile::from_os_path(path),
        masks: AttrMask::stat(),
        ..VfAttrs::default()
    };
    backend.vgetattrs_nofollow_impl(std::slice::from_mut(&mut a))?;
    Ok(a)
}

pub fn fstat_impl_default<F: MetadataFileSystem + ?Sized>(
    backend: &mut F,
    tcf: &VfFile,
) -> VfResult<VfAttrs> {
    let mut a = VfAttrs {
        file: tcf.clone(),
        masks: AttrMask::stat(),
        ..VfAttrs::default()
    };
    backend.vgetattrs_impl(std::slice::from_mut(&mut a))?;
    Ok(a)
}

pub fn exists_impl_default<F: MetadataFileSystem + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<bool> {
    match backend.lstat_impl(path) {
        Ok(_) => Ok(true),
        Err(e) if e.err_no() == ERR_NOENT => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn file_type_impl_default<F: MetadataFileSystem + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<VfType> {
    Ok(backend.lstat_impl(path)?.ftype)
}

pub fn listdir_page_impl_default<F: DirectoryFileSystem + ?Sized>(
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

pub fn directory_page_batch_size_default<F: DirectoryFileSystem + ?Sized>(_backend: &F) -> usize {
    1
}

pub fn vlistdir_pages_impl_default<F: DirectoryFileSystem + ?Sized>(
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

pub fn walk_impl_default<F: TraversalFileSystem + ?Sized>(
    backend: &mut F,
    root: &Path,
    masks: AttrMask,
    sort: &mut dyn FnMut(&Path, &mut Vec<VfAttrs>),
) -> VfResult<Vec<WalkEntry>> {
    backend.walk_with_options_impl(root, masks, WalkOptions::default(), sort)
}

pub fn walk_with_options_impl_default<F: TraversalFileSystem + ?Sized>(
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

pub fn vlistdirs_impl_default<F: TraversalFileSystem + ?Sized>(
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

pub fn visit_dir_impl_default<F: TraversalFileSystem + ?Sized>(
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

pub fn unlink_impl_default<F: NamespaceFileSystem + ?Sized>(
    backend: &mut F,
    pathname: &Path,
) -> VfResult<()> {
    backend.vremove_impl(&[VfFile::from_os_path(pathname)])
}

pub fn vunlink_impl_default<F: NamespaceFileSystem + ?Sized>(
    backend: &mut F,
    pathnames: &[&Path],
) -> VfRes {
    let files: Vec<VfFile> = pathnames.iter().map(|p| VfFile::from_os_path(p)).collect();
    backend.vremove_impl(&files)
}

pub fn mkdir_raw_impl_default<F: NamespaceFileSystem + ?Sized>(
    backend: &mut F,
    path: &Path,
    mode: u32,
) -> VfResult<()> {
    let a = VfAttrs {
        file: VfFile::from_os_path(path),
        masks: AttrMask::MODE,
        mode,
        ..VfAttrs::default()
    };
    backend.vmkdir_impl(std::slice::from_ref(&a))
}

pub fn ensure_dir_impl_default<F: NamespaceFileSystem + ?Sized>(
    backend: &mut F,
    dir: &Path,
    mode: u32,
) -> VfResult<()> {
    use std::path::Component;
    let mut so_far = PathBuf::new();
    for comp in backend.abs_path(dir).components() {
        if let Component::Normal(part) = comp {
            so_far.push(part);
            let full = Path::new("/").join(&so_far);
            match backend.mkdir_raw_impl(&full, mode) {
                Ok(()) => {}
                Err(e) if e.err_no() == ERR_EXIST => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

pub fn symlink_raw_impl_default<F: LinkFileSystem + ?Sized>(
    backend: &mut F,
    oldpath: &Path,
    newpath: &Path,
) -> VfResult<()> {
    backend.vsymlink_impl(
        std::slice::from_ref(&oldpath),
        std::slice::from_ref(&newpath),
    )
}

pub fn readlink_raw_impl_default<F: LinkFileSystem + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<Vec<u8>> {
    take_single_result(
        "readlink_raw_impl",
        backend.vreadlink_impl(std::slice::from_ref(&path))?,
    )
}

pub fn vcopy_impl_default<F: CopyFileSystem + ?Sized>(
    backend: &mut F,
    pairs: &[ExtentPair],
    options: CopyOption,
) -> VfRes {
    if pairs.is_empty() {
        return Ok(());
    }
    if !options.follows_source_symlinks() {
        return Err(VfError::unsupported(0));
    }
    backend.vcopy_data_impl(pairs)
}

pub fn vread_all_impl_default<F: ReadWorkflowFileSystem + ?Sized>(
    backend: &mut F,
    files: &[VfFile],
) -> VfResult<Vec<Vec<u8>>> {
    backend.vread_all_with_options_impl(files, ReadAllOptions::default())
}

pub fn vread_all_with_options_impl_default<F: ReadWorkflowFileSystem + ?Sized>(
    backend: &mut F,
    files: &[VfFile],
    options: ReadAllOptions,
) -> VfResult<Vec<Vec<u8>>> {
    let mut out: Vec<Vec<u8>> = files.iter().map(|_| Vec::new()).collect();
    let mut total = 0usize;
    let mut limit_error = None;
    let stream_budget = options
        .total_byte_limit()
        .clamp(1, DEFAULT_READ_ALLV_MAX_TOTAL_BYTES);
    let chunk_size = stream_budget.min(1024 * 1024);
    backend.vstream_impl(
        files,
        chunk_size,
        stream_budget,
        &mut |index, _, data, _| {
            let Some(next_total) = total.checked_add(data.len()) else {
                limit_error = Some(index);
                return false;
            };
            if next_total > options.total_byte_limit() {
                limit_error = Some(index);
                return false;
            }
            out[index].extend_from_slice(data);
            total = next_total;
            true
        },
    )?;
    if let Some(index) = limit_error {
        return Err(VfError::failure(index, libc::EFBIG as u32));
    }
    Ok(out)
}

pub fn vstream_impl_default<F: ReadWorkflowFileSystem + ?Sized>(
    backend: &mut F,
    files: &[VfFile],
    chunk_size: usize,
    memory_limit: usize,
    cb: &mut ReadStreamCallback<'_>,
) -> VfRes {
    use std::collections::VecDeque;

    if chunk_size == 0 || memory_limit == 0 {
        return Err(VfError::failure(0, ERR_INVAL));
    }
    let mut pending: VecDeque<usize> = (0..files.len()).collect();
    let mut offsets = vec![0u64; files.len()];
    while !pending.is_empty() {
        let mut budget = memory_limit;
        let mut batch_indices = Vec::new();
        let mut reads = Vec::new();
        while budget > 0 && !pending.is_empty() {
            let index = pending.pop_front().expect("pending was non-empty");
            let length = chunk_size.min(budget);
            reads.push(ReadOp::at(files[index].clone(), offsets[index], length));
            batch_indices.push(index);
            budget -= length;
        }
        let results = backend.vread_impl(&reads).map_err(|error| {
            error.index().map_or(error.clone(), |local_index| {
                batch_indices
                    .get(local_index)
                    .copied()
                    .map_or(error.clone(), |index| error.with_index(index))
            })
        })?;
        validate_read_results("vstream_impl", &reads, &results).map_err(|error| {
            error.index().map_or(error.clone(), |local_index| {
                batch_indices
                    .get(local_index)
                    .copied()
                    .map_or(error.clone(), |index| error.with_index(index))
            })
        })?;
        for (batch_index, result) in results.into_iter().enumerate() {
            let index = batch_indices[batch_index];
            let offset = offsets[index];
            let eof = result.eof;
            offsets[index] = offset
                .checked_add(result.data.len() as u64)
                .ok_or_else(|| VfError::failure(index, libc::EOVERFLOW as u32))?;
            if !cb(index, offset, &result.data, eof) {
                return Ok(());
            }
            if !eof {
                pending.push_back(index);
            }
        }
    }
    Ok(())
}

pub fn before_remove_type_default<F: RemovalFileSystem + ?Sized>(
    _backend: &mut F,
    _index: usize,
) -> VfResult<()> {
    Ok(())
}

pub fn remove_paths_impl_default<F: RemovalFileSystem + ?Sized>(
    backend: &mut F,
    objs: &[&Path],
    recursive: bool,
) -> VfRes {
    backend.remove_paths_with_options_impl(objs, recursive, RemoveOptions::default())
}

pub fn remove_paths_with_options_impl_default<F: RemovalFileSystem + ?Sized>(
    backend: &mut F,
    objs: &[&Path],
    recursive: bool,
    options: RemoveOptions,
) -> VfRes {
    if options != RemoveOptions::default() {
        return Err(VfError::unsupported(0));
    }
    if objs.is_empty() {
        return Ok(());
    }
    if !recursive {
        let files: Vec<VfFile> = objs.iter().map(|p| VfFile::from_os_path(p)).collect();
        return backend.vremove_impl(&files);
    }

    for (root_index, root) in objs.iter().enumerate() {
        backend.before_remove_type(root_index)?;
        // Classify the root (no-follow); a non-directory is just removed.
        let mut root_attrs = VfAttrs {
            file: VfFile::from_os_path(root),
            masks: AttrMask::default(),
            ..VfAttrs::default()
        };
        backend
            .vgetattrs_nofollow_impl(std::slice::from_mut(&mut root_attrs))
            .map_err(|error| error.map_index(|_| root_index))?;
        if root_attrs.ftype != VfType::Directory {
            backend
                .vremove_impl(&[VfFile::from_os_path(root)])
                .map_err(|error| error.map_index(|_| root_index))?;
            continue;
        }

        // `dir_levels[k]` holds the directories at depth `k`; they are
        // removed in reverse once deeper levels are gone.
        let mut dir_levels: Vec<Vec<PathBuf>> = Vec::new();
        let mut frontier: Vec<PathBuf> = vec![root.to_path_buf()];
        while !frontier.is_empty() {
            let refs: Vec<&Path> = frontier.iter().map(PathBuf::as_path).collect();
            let mut per_dir: Vec<Vec<VfAttrs>> = (0..refs.len()).map(|_| Vec::new()).collect();
            {
                let index_of: std::collections::HashMap<&Path, usize> =
                    refs.iter().enumerate().map(|(i, p)| (*p, i)).collect();
                let mut cb = |attrs: &VfAttrs, dir: &Path| {
                    if let Some(&slot) = index_of.get(dir) {
                        per_dir[slot].push(attrs.clone());
                    }
                    true
                };
                backend
                    .vlistdirs_impl(&refs, AttrMask::default(), 0, false, &mut cb)
                    .map_err(|error| error.map_index(|_| root_index))?;
            }

            let mut next: Vec<PathBuf> = Vec::new();
            for entries in per_dir {
                let mut files: Vec<VfFile> = Vec::new();
                for attrs in entries {
                    if attrs.ftype == VfType::Directory {
                        if let Some(path) = attrs.file.path() {
                            next.push(path.to_path_buf());
                        }
                    } else {
                        files.push(attrs.file);
                    }
                }
                if !files.is_empty() {
                    backend
                        .vremove_impl(&files)
                        .map_err(|error| error.map_index(|_| root_index))?;
                }
            }
            dir_levels.push(frontier);
            frontier = next;
        }

        for level in dir_levels.iter().rev() {
            let dirs: Vec<VfFile> = level
                .iter()
                .map(|path| VfFile::from_os_path(path))
                .collect();
            backend
                .vremove_impl(&dirs)
                .map_err(|error| error.map_index(|_| root_index))?;
        }
    }
    Ok(())
}

pub fn open_dir_impl_default<F: RemovalFileSystem + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<VfDir> {
    let mut attrs = VfAttrs {
        file: VfFile::from_os_path(path),
        masks: AttrMask::default(),
        ..VfAttrs::default()
    };
    backend.vgetattrs_nofollow_impl(std::slice::from_mut(&mut attrs))?;
    if attrs.ftype != VfType::Directory {
        return Err(VfError::failure(0, ERR_NOTDIR));
    }
    Ok(VfDir::Path(path.to_path_buf()))
}

pub fn remove_dir_contents_handle_impl_default<F: RemovalFileSystem + ?Sized>(
    backend: &mut F,
    dir: &VfDir,
) -> VfRes {
    backend.remove_dir_contents_handle_with_options_impl(dir, RemoveOptions::default())
}

pub fn remove_dir_contents_handle_with_options_impl_default<F: RemovalFileSystem + ?Sized>(
    backend: &mut F,
    dir: &VfDir,
    options: RemoveOptions,
) -> VfRes {
    match dir {
        VfDir::Path(path) => backend.remove_dir_contents_path_with_options_impl(path, options),
        _ => Err(VfError::unsupported(0)),
    }
}

pub fn close_dir_impl_default<F: RemovalFileSystem + ?Sized>(
    _backend: &mut F,
    _dir: &VfDir,
) -> VfResult<()> {
    Ok(())
}

pub fn remove_dir_contents_path_impl_default<F: RemovalFileSystem + ?Sized>(
    backend: &mut F,
    dir: &Path,
) -> VfRes {
    backend.remove_dir_contents_path_with_options_impl(dir, RemoveOptions::default())
}

pub fn remove_dir_contents_path_with_options_impl_default<F: RemovalFileSystem + ?Sized>(
    backend: &mut F,
    dir: &Path,
    options: RemoveOptions,
) -> VfRes {
    if options != RemoveOptions::default() {
        return Err(VfError::unsupported(0));
    }
    let entries = backend
        .listdir_impl(dir, AttrMask::default(), 0, false)
        .map_err(|error| error.with_index(0))?;
    let mut files: Vec<VfFile> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for attrs in entries {
        if attrs.ftype == VfType::Directory {
            if let Some(path) = attrs.file.path() {
                dirs.push(path.to_path_buf());
            }
        } else {
            files.push(attrs.file);
        }
    }
    if !files.is_empty() {
        backend
            .vremove_impl(&files)
            .map_err(|error| error.with_index(0))?;
    }
    for sub in dirs {
        backend
            .remove_paths_impl(&[sub.as_path()], true)
            .map_err(|error| error.with_index(0))?;
    }
    Ok(())
}

pub fn ensure_empty_dir_impl_default<F: RemovalFileSystem + ?Sized>(
    backend: &mut F,
    dir: &Path,
) -> VfRes {
    match backend.mkdir_raw_impl(dir, 0o777) {
        Ok(()) => Ok(()),
        Err(error) if error.err_no() == ERR_EXIST => {
            let mut attrs = VfAttrs {
                file: VfFile::from_os_path(dir),
                masks: AttrMask::default(),
                ..VfAttrs::default()
            };
            backend.vgetattrs_nofollow_impl(std::slice::from_mut(&mut attrs))?;
            if attrs.ftype != VfType::Directory {
                return Err(VfError::failure(0, ERR_NOTDIR));
            }
            backend.remove_dir_contents_path_impl(dir)
        }
        Err(error) => Err(error),
    }
}

pub fn vopen_typed_default<F: VectorFileSystem + ?Sized>(
    backend: &mut F,
    requests: &[OpenRequest],
) -> VfResult<Vec<VfFile>> {
    let paths: Vec<&std::path::Path> = requests
        .iter()
        .map(|request| request.path.as_path())
        .collect();
    let flags = translate_open_flags(requests)?;
    let modes: Vec<u32> = requests.iter().map(|request| request.mode).collect();
    backend.vopen_raw_impl(&paths, &flags, &modes)
}

pub fn native_read_file_impl_default<F: ReadWorkflowFileSystem + ?Sized>(
    backend: &mut F,
    file: &VfFile,
    max_bytes: usize,
) -> VfResult<Vec<u8>> {
    backend
        .vread_all_with_options_impl(
            std::slice::from_ref(file),
            ReadAllOptions::new().max_total_bytes(max_bytes),
        )
        .and_then(|mut results| {
            if results.len() != 1 {
                return Err(VfError::transport(
                    None,
                    "read_file backend returned an invalid result count",
                ));
            }
            let data = results.pop().expect("validated result count");
            if data.len() > max_bytes {
                return Err(VfError::client(0, libc::EFBIG as u32));
            }
            Ok(data)
        })
}

pub fn native_open_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    request: &OpenRequest,
) -> VfResult<VfFile> {
    backend
        .open_raw_impl(
            request.path.as_path(),
            vfsi_core::open_flags_to_libc(request.flags)?,
            request.mode,
        )
        .map_err(|error| error.with_context("open", &request.path))
}

pub fn native_read_impl_default<F: VectorFileSystem + ?Sized>(
    backend: &mut F,
    request: &ReadOp,
) -> VfResult<ReadResult> {
    let mut results = backend.vread_impl(std::slice::from_ref(request))?;
    validate_read_results("read_impl", std::slice::from_ref(request), &results)?;
    Ok(results.pop().expect("validated one result"))
}

pub fn native_read_into_impl_default<F: VectorFileSystem + ?Sized>(
    backend: &mut F,
    request: &ReadOp,
    buffer: &mut [u8],
) -> VfResult<ReadIntoResult> {
    let mut results = backend.vread_into_impl(std::slice::from_ref(request), &mut [buffer])?;
    validate_read_into_results("read_into_impl", std::slice::from_ref(request), &results)?;
    Ok(results.pop().expect("validated one result"))
}

pub fn native_write_impl_default<F: VectorFileSystem + ?Sized>(
    backend: &mut F,
    request: WriteOpRef<'_>,
) -> VfResult<WriteResult> {
    let mut results = backend.vwrite_impl(std::slice::from_ref(&request))?;
    validate_write_results("write_impl", std::slice::from_ref(&request), &results)?;
    Ok(results.pop().expect("validated one result"))
}

pub fn native_seek_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    file: &VfFile,
    position: std::io::SeekFrom,
) -> VfResult<u64> {
    let (offset, whence) = match position {
        std::io::SeekFrom::Start(offset) => (
            i64::try_from(offset).map_err(|_| VfError::failure(0, libc::EOVERFLOW as u32))?,
            SeekFrom::Set,
        ),
        std::io::SeekFrom::End(offset) => (offset, SeekFrom::End),
        std::io::SeekFrom::Current(offset) => (offset, SeekFrom::Cur),
    };
    u64::try_from(backend.seek_raw_impl(file, offset, whence)?)
        .map_err(|_| VfError::failure(0, ERR_INVAL))
}

pub fn native_metadata_impl_default<F: MetadataFileSystem + ?Sized>(
    backend: &mut F,
    query: MetadataQuery,
) -> VfResult<VfAttrs> {
    let path = query.file.path().map(std::path::Path::to_path_buf);
    let mut attrs = VfAttrs {
        file: query.file,
        masks: query.attributes,
        ..VfAttrs::default()
    };
    let result = if query.follow_symlinks {
        backend.vgetattrs_impl(std::slice::from_mut(&mut attrs))
    } else {
        backend.vgetattrs_nofollow_impl(std::slice::from_mut(&mut attrs))
    };
    result.map_err(|error| match path {
        Some(path) => error.with_context("metadata", path),
        None => error,
    })?;
    Ok(attrs)
}

pub fn native_set_attributes_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    update: SetAttributes,
) -> VfResult<()> {
    let follow = update.follow_symlinks;
    let path = update.file.path().map(std::path::Path::to_path_buf);
    let result = backend.vsetattrs_impl(vec![update], follow);
    result.map_err(|error| match path {
        Some(path) => error.with_context("set_attributes", path),
        None => error,
    })
}

pub fn native_metadata_path_impl_default<F: MetadataFileSystem + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
    follow: bool,
) -> VfResult<Metadata> {
    let mut attributes = VfAttrs {
        file: VfFile::from_os_path(path),
        masks: metadata_mask(),
        ..VfAttrs::default()
    };
    let result = if follow {
        backend.vgetattrs_impl(std::slice::from_mut(&mut attributes))
    } else {
        backend.vgetattrs_nofollow_impl(std::slice::from_mut(&mut attributes))
    };
    result
        .map_err(|error| error.with_context("metadata", path))
        .map(|()| vfsi_core::metadata_from_attrs(attributes))
}

pub fn native_set_metadata_path_impl_default<F: MetadataFileSystem + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
    update: MetadataUpdate,
    follow: bool,
) -> VfResult<()> {
    let mut attributes = SetAttributes::new(VfFile::from_os_path(path));
    attributes.follow_symlinks = follow;
    attributes.mode = update.permissions.map(Permissions::mode);
    attributes.size = update.len;
    attributes.uid = update.uid;
    attributes.gid = update.gid;
    attributes.atime = update.accessed.map(system_time_parts).transpose()?;
    attributes.mtime = update.modified.map(system_time_parts).transpose()?;
    backend
        .set_attributes_impl(attributes)
        .map_err(|error| error.with_context("set_metadata", path))
}

pub fn native_create_dir_impl_default<F: NamespaceFileSystem + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
    mode: u32,
) -> VfResult<()> {
    backend
        .mkdir_raw_impl(path, mode)
        .map_err(|error| error.with_context("create_dir", path))
}

pub fn native_read_dir_impl_default<F: DirectoryFileSystem + ?Sized>(
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

pub fn native_read_dir_page_impl_default<F: DirectoryFileSystem + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
    cursor: Option<DirPageCursor>,
    page_size: usize,
    max_entries: usize,
) -> VfResult<(Vec<DirEntry>, Option<DirPageCursor>)> {
    backend.read_dir_page_with_fields_impl(path, metadata_mask(), cursor, page_size, max_entries)
}

pub fn native_read_dir_page_with_fields_impl_default<F: DirectoryFileSystem + ?Sized>(
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

pub fn native_remove_impl_default<F: RemovalFileSystem + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
    recursive: bool,
) -> VfResult<()> {
    backend
        .remove_paths_impl(&[path], recursive)
        .map_err(|error| error.with_context("remove", path))
}

pub fn native_remove_dir_contents_impl_default<F: RemovalFileSystem + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
) -> VfResult<()> {
    backend
        .remove_dir_contents_path_impl(path)
        .map_err(|error| error.with_context("remove_dir_contents", path))
}

pub fn native_rename_impl_default<F: NamespaceFileSystem + ?Sized>(
    backend: &mut F,
    from: &std::path::Path,
    to: &std::path::Path,
) -> VfResult<()> {
    backend
        .vrename_impl(&[(VfFile::from_os_path(from), VfFile::from_os_path(to))])
        .map_err(|error| error.with_context("rename", from))
}

pub fn native_symlink_impl_default<F: LinkFileSystem + ?Sized>(
    backend: &mut F,
    target: &std::path::Path,
    link: &std::path::Path,
) -> VfResult<()> {
    backend
        .symlink_raw_impl(target, link)
        .map_err(|error| error.with_context("symlink", link))
}

pub fn native_hard_link_impl_default<F: LinkFileSystem + ?Sized>(
    backend: &mut F,
    source: &std::path::Path,
    link: &std::path::Path,
) -> VfResult<()> {
    backend
        .vhardlink_impl(&[source], &[link])
        .map_err(|error| error.with_context("hard_link", link))
}

pub fn native_read_link_impl_default<F: LinkFileSystem + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
) -> VfResult<std::path::PathBuf> {
    backend
        .readlink_raw_impl(path)
        .map(bytes_to_path)
        .map_err(|error| error.with_context("read_link", path))
}

pub fn native_copy_impl_default<F: CopyFileSystem + ?Sized>(
    backend: &mut F,
    source: &std::path::Path,
    destination: &std::path::Path,
) -> VfResult<()> {
    backend
        .vcopy_impl(
            &[ExtentPair::from_os_paths(source, 0, destination, 0, None)],
            vfsi_core::CopyOption::new(),
        )
        .map_err(|error| error.with_context("copy", source))
}

pub fn remove_tree<F: RemovalFileSystem + ?Sized>(backend: &mut F, path: &Path) -> VfResult<()> {
    backend.remove_paths_impl(&[path], true)
}
pub fn vsetattrs_typed_default<F: MetadataFileSystem + ?Sized>(
    backend: &mut F,
    updates: Vec<SetAttributes>,
    follow: bool,
) -> VfResult<()> {
    let attrs: Vec<VfAttrs> = updates
        .into_iter()
        .map(SetAttributes::into_legacy)
        .collect();
    if follow {
        backend.vsetattrs_raw_impl(&attrs)
    } else {
        backend.vsetattrs_raw_nofollow_impl(&attrs)
    }
}

pub fn read_file_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    file: &VfFile,
    max_bytes: usize,
) -> VfResult<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let remaining = max_bytes.saturating_sub(output.len());
        let request = ReadOp::new(
            file.clone(),
            VfOffset::At(output.len() as u64),
            remaining.clamp(1, 1024 * 1024),
        );
        let result = backend.read_impl(&request)?;
        validate_read_results(
            "read_file",
            std::slice::from_ref(&request),
            std::slice::from_ref(&result),
        )?;
        if result.data.len() > remaining {
            return Err(VfError::client(0, libc::EFBIG as u32));
        }
        if result.data.is_empty() && !result.eof {
            return Err(VfError::client(0, ERR_IO));
        }
        output.extend_from_slice(&result.data);
        if result.eof {
            return Ok(output);
        }
    }
}
pub fn read_into_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    request: &ReadOp,
    buffer: &mut [u8],
) -> VfResult<ReadIntoResult> {
    if request.length != buffer.len() {
        return Err(VfError::client(0, ERR_INVAL));
    }
    let result = backend.read_impl(request)?;
    if result.data.len() > buffer.len() {
        return Err(VfError::client(0, ERR_IO));
    }
    validate_read_results(
        "read_into_impl",
        std::slice::from_ref(request),
        std::slice::from_ref(&result),
    )?;
    buffer[..result.data.len()].copy_from_slice(&result.data);
    Ok(ReadIntoResult {
        file: result.file,
        offset: result.offset,
        read: result.data.len(),
        eof: result.eof,
    })
}
pub fn vsetattrs_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    updates: Vec<SetAttributes>,
    follow: bool,
) -> VfResult<()> {
    match updates.len() {
        0 => Ok(()),
        1 => {
            let mut update = updates.into_iter().next().expect("singleton");
            update.follow_symlinks = follow;
            backend.set_attributes_impl(update)
        }
        _ => Err(VfError::unsupported(0)),
    }
}
