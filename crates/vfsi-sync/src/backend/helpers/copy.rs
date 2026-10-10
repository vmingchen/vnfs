use super::*;

pub fn vcopy_impl_default<F: VectorBackend + ?Sized>(
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

/// Shared iterative traversal; callers retain copy strategy and error attribution.
pub fn copy_tree_with<F: VectorBackend + ?Sized>(
    backend: &mut F,
    source: &Path,
    destination: &Path,
    symlinks: bool,
    masks: AttrMask,
    mut copy_file: impl FnMut(&mut F, &ExtentPair) -> VfRes,
    map_error: impl Fn(VfError) -> VfError,
) -> VfRes {
    if !backend.exists_impl(destination)? {
        backend
            .ensure_dir_impl(destination, 0o755)
            .map_err(&map_error)?;
    }
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf())];
    while let Some((source, destination)) = pending.pop() {
        let entries = backend.listdir_impl(&source, masks, 0, false)?;
        let mut directories = Vec::new();
        for entry in entries {
            let name = entry
                .file
                .path()
                .and_then(Path::file_name)
                .ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
            let source_child = source.join(name);
            let destination_child = destination.join(name);
            if entry.ftype == VfType::Directory {
                backend
                    .ensure_dir_impl(&destination_child, 0o755)
                    .map_err(&map_error)?;
                directories.push((source_child, destination_child));
            } else if entry.ftype == VfType::Symlink && symlinks {
                let target = backend
                    .readlink_raw_impl(&source_child)
                    .map_err(&map_error)?;
                backend
                    .symlink_raw_impl(&bytes_to_path(target), &destination_child)
                    .map_err(&map_error)?;
            } else {
                let pair = ExtentPair::from_os_paths(&source_child, 0, &destination_child, 0, None);
                copy_file(backend, &pair).map_err(&map_error)?;
            }
        }
        pending.extend(directories.into_iter().rev());
    }
    Ok(())
}
