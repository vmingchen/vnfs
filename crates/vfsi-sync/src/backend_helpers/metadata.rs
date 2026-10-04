use super::*;

pub fn stat_impl_default<F: Backend + ?Sized>(backend: &mut F, path: &Path) -> VfResult<VfAttrs> {
    let mut a = VfAttrs {
        file: VfFile::from_os_path(path),
        masks: AttrMask::stat(),
        ..VfAttrs::default()
    };
    backend.vgetattrs_impl(std::slice::from_mut(&mut a))?;
    Ok(a)
}

pub fn lstat_impl_default<F: Backend + ?Sized>(backend: &mut F, path: &Path) -> VfResult<VfAttrs> {
    let mut a = VfAttrs {
        file: VfFile::from_os_path(path),
        masks: AttrMask::stat(),
        ..VfAttrs::default()
    };
    backend.vgetattrs_nofollow_impl(std::slice::from_mut(&mut a))?;
    Ok(a)
}

pub fn fstat_impl_default<F: Backend + ?Sized>(backend: &mut F, tcf: &VfFile) -> VfResult<VfAttrs> {
    let mut a = VfAttrs {
        file: tcf.clone(),
        masks: AttrMask::stat(),
        ..VfAttrs::default()
    };
    backend.vgetattrs_impl(std::slice::from_mut(&mut a))?;
    Ok(a)
}

pub fn exists_impl_default<F: Backend + ?Sized>(backend: &mut F, path: &Path) -> VfResult<bool> {
    match backend.lstat_impl(path) {
        Ok(_) => Ok(true),
        Err(e) if e.err_no() == ERR_NOENT => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn file_type_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<VfType> {
    Ok(backend.lstat_impl(path)?.ftype)
}

pub fn native_metadata_impl_default<F: Backend + ?Sized>(
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

pub fn native_metadata_path_impl_default<F: Backend + ?Sized>(
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

pub fn native_set_metadata_path_impl_default<F: Backend + ?Sized>(
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

pub fn vsetattrs_typed_default<F: Backend + ?Sized>(
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
