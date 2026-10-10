use super::*;

pub fn stat_impl_default<F: VectorBackend + ?Sized>(
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

pub fn lstat_impl_default<F: VectorBackend + ?Sized>(
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

pub fn fstat_impl_default<F: VectorBackend + ?Sized>(
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

pub fn exists_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<bool> {
    match backend.lstat_impl(path) {
        Ok(_) => Ok(true),
        Err(e) if e.err_no() == ERR_NOENT => Ok(false),
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
    target: Target<'_, VfFile>,
    options: vfsi_core::api::AttrsOptions,
) -> VfResult<VfAttrs> {
    let file = match target {
        Target::Path(path) => VfFile::from_os_path(path),
        Target::File(file) => file.clone(),
    };
    let path = match target {
        Target::Path(path) => Some(path),
        Target::File(file) => file.path(),
    };
    let mut attrs = VfAttrs {
        file,
        masks: options.requested_attributes(),
        ..VfAttrs::default()
    };
    let result = if options.follows_symlinks() {
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

pub fn native_metadata_path_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
    follow: bool,
) -> VfResult<Attrs> {
    native_metadata_impl_default(
        backend,
        Target::Path(path),
        vfsi_core::api::AttrsOptions::new()
            .fields(metadata_mask())
            .follow_symlinks(follow),
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
// public mutation type. Convert and validate every operation before dispatch.
fn setattrs_to_legacy(op: &SetAttrsOp<Target<'_, VfFile>>) -> VfResult<VfAttrs> {
    if op.requested_uid() == Some(u32::MAX) || op.requested_gid() == Some(u32::MAX) {
        return Err(VfError::client(0, ERR_INVAL));
    }
    let mut attrs = VfAttrs {
        file: match op.target() {
            Target::Path(path) => VfFile::from_os_path(path),
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
    if let Some(time) = op.requested_accessed() {
        (attrs.atime_sec, attrs.atime_nsec) = system_time_parts(time)?;
        attrs.masks |= AttrMask::ATIME;
    }
    if let Some(time) = op.requested_modified() {
        (attrs.mtime_sec, attrs.mtime_nsec) = system_time_parts(time)?;
        attrs.masks |= AttrMask::MTIME;
    }
    Ok(attrs)
}

pub fn vsetattrs_typed_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    updates: &[SetAttrsOp<Target<'_, VfFile>>],
) -> VfResult<()> {
    let attrs = updates
        .iter()
        .enumerate()
        .map(|(index, op)| {
            setattrs_to_legacy(op).map_err(|error| {
                let error = error.with_index(index);
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
        .collect::<VfResult<Vec<_>>>()?;
    let mut start = 0;
    while start < updates.len() {
        let follow = updates[start].follows_symlinks();
        let count = updates[start..]
            .iter()
            .take_while(|op| op.follows_symlinks() == follow)
            .count();
        let result = if follow {
            backend.vsetattrs_raw_impl(&attrs[start..start + count])
        } else {
            backend.vsetattrs_raw_nofollow_impl(&attrs[start..start + count])
        };
        result.map_err(|error| match error.index() {
            Some(index) if index < count => error.with_index(start + index),
            Some(_) => VfError::transport(None, "setattrs backend returned an invalid error index"),
            None => error,
        })?;
        start += count;
    }
    Ok(())
}
