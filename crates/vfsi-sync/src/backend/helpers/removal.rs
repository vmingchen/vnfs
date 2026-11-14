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
) -> vfsi_core::VecResult<()> {
    backend.remove_paths_with_options_impl(
        objs,
        if recursive {
            vfsi_core::api::RemoveMode::Tree
        } else {
            vfsi_core::api::RemoveMode::Entry
        },
        RemoveOptions::default(),
    )
}

// Normalize one native unit stage before consuming any of its payloads.
fn directory_stage_results(
    result: vfsi_core::VecResult<()>,
    count: usize,
) -> Vec<vfsi_core::OpResult<()>> {
    use vfsi_core::OpError;
    let mut results = match result {
        Ok(results) if results.len() <= count => results,
        Ok(_) => {
            return (0..count)
                .map(|_| {
                    Err(OpError::unknown(
                        vfsi_core::internal::errors::invalid_reply(
                            "excess directory stage outcomes",
                        ),
                        None,
                    ))
                })
                .collect();
        }
        Err(cause) => {
            return vfsi_core::internal::errors::fanout(cause, count)
                .map(|cause| Err(OpError::not_attempted(cause)))
                .collect();
        }
    };
    results.resize_with(count, || {
        Err(OpError::unknown(
            vfsi_core::internal::errors::invalid_reply("missing directory stage outcome"),
            None,
        ))
    });
    for result in &mut results {
        if result
            .as_ref()
            .is_err_and(|error| error.progress().is_some())
        {
            *result = Err(OpError::unknown(
                vfsi_core::internal::errors::invalid_reply(
                    "directory stage cannot report byte progress",
                ),
                None,
            ));
        }
    }
    results
}

fn removal_stage(
    result: vfsi_core::VecResult<()>,
    count: usize,
    started: &mut bool,
) -> vfsi_core::OpResult<()> {
    use vfsi_core::{FailureState, OpError};
    let mut failure: Option<OpError> = None;
    for result in directory_stage_results(result, count) {
        match result {
            Ok(()) => *started = true,
            Err(error) => {
                *started |= error.state() != FailureState::NotAttempted;
                if failure.as_ref().is_none_or(|previous| {
                    previous.state() != FailureState::Unknown
                        && error.state() == FailureState::Unknown
                }) {
                    failure = Some(error);
                }
            }
        }
    }
    failure.map_or(Ok(()), |error| {
        Err(if *started && error.state() == FailureState::NotAttempted {
            OpError::failed(error.into_cause(), None)
        } else {
            error
        })
    })
}

pub fn remove_paths_with_options_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    objs: &[&Path],
    mode: vfsi_core::api::RemoveMode,
    options: RemoveOptions,
) -> vfsi_core::VecResult<()> {
    use vfsi_core::{FailureState, OpError, api::RemoveMode};
    if objs.is_empty() {
        return Ok(Vec::new());
    }
    if options != RemoveOptions::default() {
        return Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
    }
    let files: Vec<_> = objs
        .iter()
        .map(|path| FileOperand::from_os_path(path))
        .collect();
    if mode == RemoveMode::Entry {
        return backend.vremove_impl(&files);
    }

    let mut output: Vec<Option<vfsi_core::OpResult<()>>> = (0..objs.len()).map(|_| None).collect();
    let mut indices = Vec::new();
    let mut attrs = Vec::new();
    let mut fatal: Option<std::sync::Arc<std::io::Error>> = None;
    for (index, file) in files.iter().enumerate() {
        if let Some(cause) = &fatal {
            output[index] = Some(Err(OpError::not_attempted(
                vfsi_core::internal::errors::from_shared(cause),
            )));
            continue;
        }
        if let Err(mut cause) = backend.before_remove_type(index) {
            if vfsi_core::transport_kind(&(cause)).is_some() {
                fatal = Some(vfsi_core::internal::errors::share_in_place(&mut cause));
            }
            output[index] = Some(Err(OpError::not_attempted(cause)));
        } else {
            indices.push(index);
            attrs.push(VfAttrs {
                file: file.clone(),
                masks: AttrMask::default(),
                ..Default::default()
            });
        }
    }
    if let Some(cause) = fatal {
        for index in indices {
            output[index] = Some(Err(OpError::not_attempted(
                vfsi_core::internal::errors::from_shared(&cause),
            )));
        }
        return Ok(output.into_iter().map(Option::unwrap).collect());
    }
    let results = directory_stage_results(backend.vgetattrs_nofollow_impl(&mut attrs), attrs.len());
    let mut ready = Vec::new();
    for ((index, attrs), result) in indices.into_iter().zip(attrs).zip(results) {
        match result {
            Ok(()) if mode == RemoveMode::Contents && attrs.ftype != VfType::Directory => {
                output[index] = Some(Err(OpError::not_attempted(
                    std::io::Error::from_raw_os_error((ERR_NOTDIR) as i32),
                )));
            }
            Ok(()) => ready.push((index, attrs.ftype)),
            Err(error) => {
                output[index] = Some(Err(error));
            }
        }
    }
    if let Some(cause) = output
        .iter_mut()
        .filter_map(|slot| slot.as_mut())
        .filter_map(|result| result.as_mut().err())
        .find(|error| {
            error.state() == FailureState::Unknown
                || vfsi_core::transport_kind(error.cause()).is_some()
        })
        .map(vfsi_core::internal::errors::share_op_cause)
    {
        for slot in &mut output {
            if slot.is_none() {
                *slot = Some(Err(OpError::not_attempted(
                    vfsi_core::internal::errors::from_shared(&cause),
                )));
            }
        }
        return Ok(output.into_iter().map(Option::unwrap).collect());
    }
    // Root classification is one vector. Leaf roots also share one native
    // removal vector rather than being reduced to singleton calls.
    let leaves: Vec<_> = ready
        .iter()
        .filter_map(|&(index, kind)| (kind != VfType::Directory).then_some(index))
        .collect();
    if !leaves.is_empty() {
        let requests: Vec<_> = leaves.iter().map(|&index| files[index].clone()).collect();
        for (index, result) in leaves.into_iter().zip(directory_stage_results(
            backend.vremove_impl(&requests),
            requests.len(),
        )) {
            output[index] = Some(result);
        }
    }
    fatal = output
        .iter_mut()
        .filter_map(|slot| slot.as_mut())
        .filter_map(|result| result.as_mut().err())
        .find(|error| {
            error.state() == FailureState::Unknown
                || vfsi_core::transport_kind(error.cause()).is_some()
        })
        .map(vfsi_core::internal::errors::share_op_cause);
    for (index, kind) in ready {
        if output[index].is_some() {
            continue;
        }
        if let Some(cause) = &fatal {
            output[index] = Some(Err(OpError::not_attempted(
                vfsi_core::internal::errors::from_shared(cause),
            )));
            continue;
        }
        debug_assert_eq!(kind, VfType::Directory);
        let mut result = remove_directory_root(backend, objs[index], mode == RemoveMode::Contents);
        if let Err(error) = &mut result
            && (error.state() == FailureState::Unknown
                || vfsi_core::transport_kind(error.cause()).is_some())
        {
            fatal = Some(vfsi_core::internal::errors::share_op_cause(error));
        }
        output[index] = Some(result);
    }
    Ok(output
        .into_iter()
        .map(|slot| slot.expect("all removal roots assigned"))
        .collect())
}

// Existing breadth-first traversal, with per-root results and native vector
// barriers between directory levels. A failed root never replays a stage.
fn remove_directory_root<F: VectorBackend + ?Sized>(
    backend: &mut F,
    root: &Path,
    keep_root: bool,
) -> vfsi_core::OpResult<()> {
    use vfsi_core::OpError;
    let mut levels: Vec<Vec<PathBuf>> = Vec::new();
    let mut frontier = vec![root.to_path_buf()];
    let mut started = true; // This root is entering its directory scan.
    while !frontier.is_empty() {
        let refs: Vec<_> = frontier.iter().map(PathBuf::as_path).collect();
        let mut per_dir: Vec<Vec<VfAttrs>> = (0..refs.len()).map(|_| Vec::new()).collect();
        {
            let index_of: std::collections::HashMap<_, _> = refs
                .iter()
                .enumerate()
                .map(|(index, path)| (*path, index))
                .collect();
            backend
                .vlistdirs_impl(&refs, AttrMask::default(), 0, false, &mut |attrs, dir| {
                    if let Some(&index) = index_of.get(dir) {
                        per_dir[index].push(attrs.clone());
                    }
                    true
                })
                .map_err(|cause| OpError::failed(cause.into_cause(), None))?;
        }
        let mut next = Vec::new();
        let mut files = Vec::new();
        for entries in per_dir {
            for attrs in entries {
                if attrs.ftype == VfType::Directory {
                    if let Some(path) = attrs.file.path() {
                        next.push(path.to_path_buf());
                    }
                } else {
                    files.push(attrs.file);
                }
            }
        }
        if !files.is_empty() {
            removal_stage(backend.vremove_impl(&files), files.len(), &mut started)?;
        }
        levels.push(frontier);
        frontier = next;
    }
    for (depth, level) in levels.iter().enumerate().rev() {
        if depth == 0 && keep_root {
            continue;
        }
        let dirs: Vec<_> = level
            .iter()
            .map(|path| FileOperand::from_os_path(path))
            .collect();
        removal_stage(backend.vremove_impl(&dirs), dirs.len(), &mut started)?;
    }
    Ok(())
}

pub fn vopen_dirs_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    paths: &[&Path],
) -> vfsi_core::VecResult<VfDir> {
    use vfsi_core::OpError;
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut attrs: Vec<_> = paths
        .iter()
        .map(|path| VfAttrs {
            file: FileOperand::from_os_path(path),
            masks: AttrMask::default(),
            ..Default::default()
        })
        .collect();
    Ok(
        directory_stage_results(backend.vgetattrs_nofollow_impl(&mut attrs), paths.len())
            .into_iter()
            .zip(attrs)
            .zip(paths)
            .map(|((outcome, attrs), path)| {
                outcome.and_then(|()| {
                    if attrs.ftype == VfType::Directory {
                        Ok(VfDir::Path((*path).to_owned()))
                    } else {
                        Err(OpError::failed(
                            std::io::Error::from_raw_os_error((ERR_NOTDIR) as i32),
                            None,
                        ))
                    }
                })
            })
            .collect(),
    )
}

pub fn open_dir_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<VfDir> {
    vfsi_core::internal::collect_vec_result(backend.vopen_dirs_impl(&[path]), 1)
        .map_err(vfsi_core::IndexedError::into_cause)
        .map(|mut dirs| dirs.remove(0))
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
    vfsi_core::internal::collect_vec_result(
        backend.remove_dir_contents_handles_with_options_impl(&[dir], options),
        1,
    )
    .map_err(vfsi_core::IndexedError::into_cause)
    .map(|_| ())
}

pub fn remove_dir_contents_handles_with_options_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    dirs: &[&VfDir],
    options: RemoveOptions,
) -> vfsi_core::VecResult<()> {
    use vfsi_core::OpError;
    if dirs.is_empty() {
        return Ok(Vec::new());
    }
    let mut output: Vec<_> = (0..dirs.len()).map(|_| None).collect();
    let mut paths = Vec::new();
    let mut indices = Vec::new();
    for (index, dir) in dirs.iter().enumerate() {
        match dir {
            VfDir::Path(path) => {
                paths.push(path.as_path());
                indices.push(index);
            }
            _ => {
                output[index] = Some(Err(OpError::not_attempted(std::io::Error::from(
                    std::io::ErrorKind::Unsupported,
                ))))
            }
        }
    }
    if !paths.is_empty() {
        for (index, result) in indices.into_iter().zip(directory_stage_results(
            backend.remove_paths_with_options_impl(
                &paths,
                vfsi_core::api::RemoveMode::Contents,
                options,
            ),
            paths.len(),
        )) {
            output[index] = Some(result);
        }
    }
    Ok(output.into_iter().map(Option::unwrap).collect())
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
    vfsi_core::internal::collect_vec_result(
        backend.remove_paths_with_options_impl(
            &[dir],
            vfsi_core::api::RemoveMode::Contents,
            options,
        ),
        1,
    )
    .map_err(vfsi_core::IndexedError::into_cause)
    .map(|_| ())
}

pub fn ensure_empty_dir_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    dir: &Path,
) -> VfRes {
    match backend.mkdir_raw_impl(dir, 0o777) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut attrs = VfAttrs {
                file: FileOperand::from_os_path(dir),
                masks: AttrMask::default(),
                ..VfAttrs::default()
            };
            vfsi_core::internal::collect_vec_result(
                backend.vgetattrs_nofollow_impl(std::slice::from_mut(&mut attrs)),
                1,
            )
            .map_err(vfsi_core::IndexedError::into_cause)?;
            if attrs.ftype != VfType::Directory {
                return Err(std::io::Error::from_raw_os_error((ERR_NOTDIR) as i32));
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
    vfsi_core::internal::collect_vec_result(backend.remove_paths_impl(&[path], recursive), 1)
        .map_err(vfsi_core::IndexedError::into_cause)
        .map(|_| ())
        .map_err(|error| vfsi_core::internal::errors::with_context(error, "remove", path))
}

pub fn native_remove_dir_contents_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
) -> VfResult<()> {
    backend
        .remove_dir_contents_path_impl(path)
        .map_err(|error| {
            vfsi_core::internal::errors::with_context(error, "remove_dir_contents", path)
        })
}

pub fn remove_tree<F: VectorBackend + ?Sized>(backend: &mut F, path: &Path) -> VfResult<()> {
    vfsi_core::internal::collect_vec_result(backend.remove_paths_impl(&[path], true), 1)
        .map_err(vfsi_core::IndexedError::into_cause)
        .map(|_| ())
}
