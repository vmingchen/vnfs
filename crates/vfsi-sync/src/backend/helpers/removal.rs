use super::*;

pub fn before_remove_type_default<F: VectorBackend + ?Sized>(
    _backend: &mut F,
    _index: usize,
) -> VfResult<()> {
    Ok(())
}

pub fn remove_paths_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    objs: &[&Path],
    recursive: bool,
) -> VfRes {
    backend.remove_paths_with_options_impl(objs, recursive, RemoveOptions::default())
}

pub fn remove_paths_with_options_impl_default<F: VectorBackend + ?Sized>(
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

pub fn open_dir_impl_default<F: VectorBackend + ?Sized>(
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

pub fn remove_dir_contents_handle_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    dir: &VfDir,
) -> VfRes {
    backend.remove_dir_contents_handle_with_options_impl(dir, RemoveOptions::default())
}

pub fn remove_dir_contents_handle_with_options_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    dir: &VfDir,
    options: RemoveOptions,
) -> VfRes {
    match dir {
        VfDir::Path(path) => backend.remove_dir_contents_path_with_options_impl(path, options),
        _ => Err(VfError::unsupported(0)),
    }
}

pub fn close_dir_impl_default<F: VectorBackend + ?Sized>(
    _backend: &mut F,
    _dir: &VfDir,
) -> VfResult<()> {
    Ok(())
}

pub fn remove_dir_contents_path_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    dir: &Path,
) -> VfRes {
    backend.remove_dir_contents_path_with_options_impl(dir, RemoveOptions::default())
}

pub fn remove_dir_contents_path_with_options_impl_default<F: VectorBackend + ?Sized>(
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

pub fn ensure_empty_dir_impl_default<F: VectorBackend + ?Sized>(
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

pub fn native_remove_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
    recursive: bool,
) -> VfResult<()> {
    backend
        .remove_paths_impl(&[path], recursive)
        .map_err(|error| error.with_context("remove", path))
}

pub fn native_remove_dir_contents_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
) -> VfResult<()> {
    backend
        .remove_dir_contents_path_impl(path)
        .map_err(|error| error.with_context("remove_dir_contents", path))
}

pub fn remove_tree<F: VectorBackend + ?Sized>(backend: &mut F, path: &Path) -> VfResult<()> {
    backend.remove_paths_impl(&[path], true)
}
