use super::*;

fn query_attributes<F: VectorBackend + ?Sized>(
    backend: &mut F,
    file: FileOperand,
    masks: AttrMask,
    follow: bool,
) -> VfResult<VfAttrs> {
    let mut attrs = VfAttrs {
        file,
        masks,
        ..Default::default()
    };
    let result = if follow {
        backend.vgetattrs_impl(std::slice::from_mut(&mut attrs))
    } else {
        backend.vgetattrs_nofollow_impl(std::slice::from_mut(&mut attrs))
    };
    vfsi_core::internal::collect_vec_result(result, 1)?;
    Ok(attrs)
}

pub fn stat_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<VfAttrs> {
    query_attributes(
        backend,
        FileOperand::from_os_path(path),
        AttrMask::stat(),
        true,
    )
}

pub fn lstat_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<VfAttrs> {
    query_attributes(
        backend,
        FileOperand::from_os_path(path),
        AttrMask::stat(),
        false,
    )
}

pub fn fstat_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    file: &FileOperand,
) -> VfResult<VfAttrs> {
    query_attributes(backend, file.clone(), AttrMask::stat(), true)
}

pub fn exists_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<bool> {
    match backend.lstat_impl(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn file_type_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<VfType> {
    Ok(backend.lstat_impl(path)?.ftype)
}

pub fn native_metadata_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    target: Target<'_, FileOperand>,
    options: vfsi_core::api::AttrsOptions,
) -> VfResult<VfAttrs> {
    let file = match target {
        Target::Path(path) => FileOperand::from_os_path(path),
        Target::File(file) => file.clone(),
    };
    let path = match target {
        Target::Path(path) => Some(path),
        Target::File(file) => file.path(),
    };
    query_attributes(
        backend,
        file,
        options.fields(),
        options.follow_final_symlink(),
    )
    .map_err(|error| match path {
        Some(path) => error.with_context("metadata", path),
        None => error,
    })
}

pub fn native_metadata_path_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
    follow: bool,
) -> VfResult<Attrs> {
    native_metadata_impl_default(
        backend,
        Target::Path(path),
        vfsi_core::api::AttrsOptions::new()
            .with_fields(metadata_mask())
            .with_follow_final_symlink(follow),
    )
    .map(vfsi_core::metadata_from_attrs)
}

pub fn native_set_metadata_path_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    op: &SetAttrsOp<&std::path::Path>,
) -> VfResult<()> {
    let path = *op.target();
    backend
        .set_attributes_impl(&op.with_target(Target::Path(path)))
        .map_err(|error| error.with_context("set_metadata", path))
}

// Wire/raw masks and timestamp tuples are execution details, not a second
// public mutation type. Validate each operation independently before dispatch.
fn setattrs_to_legacy(op: &SetAttrsOp<Target<'_, FileOperand>>) -> VfResult<VfAttrs> {
    let [accessed, modified] = vfsi_core::internal::validate_setattrs(op)?;
    let mut attrs = VfAttrs {
        file: match op.target() {
            Target::Path(path) => FileOperand::from_os_path(path),
            Target::File(file) => (*file).clone(),
        },
        ..VfAttrs::default()
    };
    if let Some(permissions) = op.requested_permissions() {
        attrs.masks |= AttrMask::MODE;
        attrs.mode = permissions.mode();
    }
    if let Some(uid) = op.requested_uid() {
        attrs.masks |= AttrMask::UID;
        attrs.uid = uid;
    }
    if let Some(gid) = op.requested_gid() {
        attrs.masks |= AttrMask::GID;
        attrs.gid = gid;
    }
    if let Some(size) = op.requested_len() {
        attrs.masks |= AttrMask::SIZE;
        attrs.size = size;
    }
    if let Some(time) = accessed {
        (attrs.atime_sec, attrs.atime_nsec) = time;
        attrs.masks |= AttrMask::ATIME;
    }
    if let Some(time) = modified {
        (attrs.mtime_sec, attrs.mtime_nsec) = time;
        attrs.masks |= AttrMask::MTIME;
    }
    Ok(attrs)
}

pub fn vsetattrs_typed_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    updates: &[SetAttrsOp<Target<'_, FileOperand>>],
) -> vfsi_core::VecResult<()> {
    use vfsi_core::OpError;
    let mut slots = (0..updates.len()).map(|_| None).collect::<Vec<_>>();
    let mut start = 0;
    while start < updates.len() {
        let follow = updates[start].follow_final_symlink();
        let count = updates[start..]
            .iter()
            .take_while(|op| op.follow_final_symlink() == follow)
            .count();
        let mut attrs = Vec::with_capacity(count);
        let mut indices = Vec::with_capacity(count);
        for (offset, op) in updates[start..start + count].iter().enumerate() {
            let index = start + offset;
            match setattrs_to_legacy(op) {
                Ok(attr) => {
                    attrs.push(attr);
                    indices.push(index);
                }
                Err(error) => slots[index] = Some(Err(OpError::not_attempted(error))),
            }
        }
        if !attrs.is_empty() {
            let result = if follow {
                backend.vsetattrs_raw_impl(&attrs)
            } else {
                backend.vsetattrs_raw_nofollow_impl(&attrs)
            };
            let mut results = match result {
                Ok(results) if results.len() <= attrs.len() => results,
                Ok(_) => attrs
                    .iter()
                    .map(|_| {
                        Err(OpError::unknown(
                            VfError::invalid_reply(None, "too many SETATTR outcomes"),
                            None,
                        ))
                    })
                    .collect(),
                Err(cause) => {
                    let cause = if cause.index().is_some_and(|index| index >= attrs.len()) {
                        VfError::invalid_reply(None, "invalid SETATTR outer error index")
                    } else {
                        cause
                    };
                    attrs
                        .iter()
                        .map(|_| Err(OpError::not_attempted(cause.clone())))
                        .collect()
                }
            };
            results.resize_with(attrs.len(), || {
                Err(OpError::unknown(
                    VfError::invalid_reply(None, "missing SETATTR outcome"),
                    None,
                ))
            });
            for (index, result) in indices.into_iter().zip(results) {
                slots[index] = Some(result);
            }
        }
        start += count;
    }
    Ok(slots
        .into_iter()
        .zip(updates)
        .map(|(slot, op)| {
            slot.expect("all attribute updates assigned")
                .map_err(|error| {
                    let path = match op.target() {
                        Target::Path(path) => Some(*path),
                        Target::File(file) => file.path(),
                    };
                    match path {
                        Some(path) => error.with_context("vsetattrs", path),
                        None => error,
                    }
                })
        })
        .collect())
}
