use super::*;

pub fn vcopy_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    pairs: &[ExtentPair],
    options: CopyOptions,
) -> VfRes {
    if pairs.is_empty() {
        return Ok(());
    }
    if !options.follow_source_symlinks() {
        return Err(VfError::unsupported(0));
    }
    backend.vcopy_data_impl(pairs)
}

/// Compatibility adapter for scalar copy strategies; traversal uses native pages.
pub fn copy_tree_with<F: VectorBackend + ?Sized>(
    backend: &mut F,
    source: &Path,
    destination: &Path,
    symlinks: bool,
    masks: AttrMask,
    mut copy_file: impl FnMut(&mut F, &ExtentPair) -> VfRes,
    map_error: impl Fn(VfError) -> VfError,
) -> VfRes {
    copy_tree_batches_with(
        backend,
        source,
        destination,
        symlinks,
        masks,
        |backend, pairs| {
            for pair in pairs {
                copy_file(backend, pair)?;
            }
            Ok(())
        },
        map_error,
    )
}

/// Preserve a backend-specific legacy listing policy without duplicating traversal.
#[allow(clippy::too_many_arguments)]
pub fn copy_tree_with_listdir<F: VectorBackend + ?Sized>(
    backend: &mut F,
    source: &Path,
    destination: &Path,
    symlinks: bool,
    masks: AttrMask,
    mut list_directory: impl FnMut(&mut F, &Path, AttrMask) -> VfResult<Vec<VfAttrs>>,
    mut copy_file: impl FnMut(&mut F, &ExtentPair) -> VfRes,
    map_error: impl Fn(VfError) -> VfError,
) -> VfRes {
    copy_tree_batches_with_pages(
        backend,
        source,
        destination,
        symlinks,
        masks,
        |backend, path, masks, cursor| {
            let mut entries = match cursor {
                Some(cursor) => cursor.into_state::<std::vec::IntoIter<VfAttrs>>()?,
                None => list_directory(backend, path, masks)?.into_iter(),
            };
            let page: Vec<_> = entries.by_ref().take(256).collect();
            let children = (0..page.len()).map(|_| None).collect();
            let next = (entries.len() != 0).then(|| DirPageCursor::new(entries));
            Ok((page, next, children))
        },
        |backend, pairs| {
            for pair in pairs {
                copy_file(backend, pair)?;
            }
            Ok(())
        },
        map_error,
    )
}

/// Copy with bounded native directory pages and contiguous file-copy vectors.
/// Child seeds retain native directory identity; scalar strategies can use
/// copy_tree_with. Failed copy vectors are never replayed.
pub fn copy_tree_batches_with<F: VectorBackend + ?Sized>(
    backend: &mut F,
    source: &Path,
    destination: &Path,
    symlinks: bool,
    masks: AttrMask,
    copy_files: impl FnMut(&mut F, &[ExtentPair]) -> VfRes,
    map_error: impl Fn(VfError) -> VfError,
) -> VfRes {
    copy_tree_batches_with_pages(
        backend,
        source,
        destination,
        symlinks,
        masks,
        |backend, path, masks, cursor| {
            let mut pages =
                backend.vlistdir_pages_impl(&[path], masks, vec![cursor], 256, usize::MAX, true)?;
            if pages.len() != 1 {
                return Err(VfError::invalid_reply(None, "invalid tree-copy page count"));
            }
            Ok(pages.pop().expect("validated one page"))
        },
        copy_files,
        map_error,
    )
}

/// Shared traversal for backends requiring a custom native page policy.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)] // Native listing/copy strategies share one traversal.
pub fn copy_tree_batches_with_pages<F: VectorBackend + ?Sized>(
    backend: &mut F,
    source: &Path,
    destination: &Path,
    symlinks: bool,
    masks: AttrMask,
    mut page: impl FnMut(
        &mut F,
        &Path,
        AttrMask,
        Option<DirPageCursor>,
    ) -> VfResult<BackendDirectoryPage>,
    mut copy_files: impl FnMut(&mut F, &[ExtentPair]) -> VfRes,
    map_error: impl Fn(VfError) -> VfError,
) -> VfRes {
    let source_abs = backend.abs_path(source);
    let destination_abs = backend.abs_path(destination);
    if source_abs.starts_with(&destination_abs) || destination_abs.starts_with(&source_abs) {
        return Err(map_error(VfError::from_kind(
            0,
            std::io::ErrorKind::InvalidInput,
        )));
    }
    let directory_mode = if backend
        .capabilities()
        .contains(Capabilities::POSIX_METADATA)
    {
        0o755
    } else {
        0o777
    };
    // Also reject an existing non-directory destination, even for an empty source.
    backend
        .ensure_dir_impl(destination, directory_mode)
        .map_err(&map_error)?;
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf(), None)];
    let mut copies = Vec::with_capacity(32);
    while let Some((source, destination, mut cursor)) = pending.pop() {
        let mut directories = Vec::new();
        loop {
            let (entries, next, seeds) =
                page(backend, &source, masks, cursor).map_err(&map_error)?;
            if entries.len() > 256
                || seeds.len() != entries.len()
                || (entries.is_empty() && next.is_some())
            {
                return Err(map_error(VfError::invalid_reply(
                    None,
                    "invalid tree-copy page progress",
                )));
            }
            for (entry, seed) in entries.into_iter().zip(seeds) {
                let name = entry.file.path().and_then(Path::file_name).ok_or_else(|| {
                    map_error(VfError::from_kind(0, std::io::ErrorKind::InvalidInput))
                })?;
                let source_child = source.join(name);
                let destination_child = destination.join(name);
                if entry.ftype == VfType::Directory || (entry.ftype == VfType::Symlink && symlinks)
                {
                    if !copies.is_empty() {
                        copy_files(backend, &copies).map_err(&map_error)?;
                        copies.clear();
                    }
                    if entry.ftype == VfType::Directory {
                        backend
                            .ensure_dir_impl(&destination_child, directory_mode)
                            .map_err(&map_error)?;
                        directories.push((source_child, destination_child, seed));
                    } else {
                        let target = backend
                            .readlink_raw_impl(&source_child)
                            .map_err(&map_error)?;
                        backend
                            .symlink_raw_impl(&bytes_to_path(target), &destination_child)
                            .map_err(&map_error)?;
                    }
                } else {
                    copies.push(ExtentPair::from_os_paths(
                        &source_child,
                        0,
                        &destination_child,
                        0,
                        None,
                    ));
                    if copies.len() == 32 {
                        copy_files(backend, &copies).map_err(&map_error)?;
                        copies.clear();
                    }
                }
            }
            if !copies.is_empty() {
                copy_files(backend, &copies).map_err(&map_error)?;
                copies.clear();
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        pending.extend(directories.into_iter().rev());
    }
    Ok(())
}
