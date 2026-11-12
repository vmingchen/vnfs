//! Shared bounded listing policy; execution remains with each native engine.
use super::metadata::attrs_from_metadata;
use super::*;

pub(super) struct DirectoryPlan<'a> {
    pub(super) originals: Vec<&'a CStr>,
    pub(super) paths: Vec<PathBuf>,
}

/// Borrow input spellings and bound planning before copying mapped paths.
///
/// # Safety
/// dirs contains count readable pointers to immutable C strings for 'a.
pub(super) unsafe fn plan_directories<'a>(
    fs: &vfsi_fs,
    dirs: *const *const c_char,
    count: usize,
) -> Result<DirectoryPlan<'a>, c_int> {
    if count > vfsi_sync::DEFAULT_DIRECTORY_MAX_ENTRIES {
        return Err(libc::EFBIG);
    }
    let mut bytes = 0usize;
    let mut originals = Vec::with_capacity(count);
    for index in 0..count {
        // SAFETY: the caller provides count readable input slots.
        let pointer = unsafe { *dirs.add(index) };
        if pointer.is_null() {
            return Err(libc::EINVAL);
        }
        // SAFETY: strings remain readable and immutable for the returned plan.
        let original = unsafe { CStr::from_ptr(pointer) };
        bytes = bytes
            .checked_add(original.to_bytes().len())
            .ok_or(libc::EFBIG)?;
        if bytes > vfsi_sync::DEFAULT_DIRECTORY_MAX_PATH_BYTES {
            return Err(libc::EFBIG);
        }
        originals.push(original);
    }
    let mut paths = Vec::with_capacity(count);
    for original in &originals {
        let path = Path::new(std::ffi::OsStr::from_bytes(original.to_bytes()));
        let mapped = vpath_for(fs, path).map_err(|error| vf_code(&error))?;
        bytes = bytes
            .checked_add(mapped.as_os_str().len())
            .ok_or(libc::EFBIG)?;
        if bytes > vfsi_sync::DEFAULT_DIRECTORY_MAX_PATH_BYTES {
            return Err(libc::EFBIG);
        }
        paths.push(mapped);
    }
    Ok(DirectoryPlan { originals, paths })
}

pub(super) struct ListingBudget<'a> {
    options: &'a vfsi_listing_options,
    entries: usize,
    bytes: usize,
}
impl<'a> ListingBudget<'a> {
    pub(super) fn new(options: &'a vfsi_listing_options) -> Self {
        Self {
            options,
            entries: 0,
            bytes: 0,
        }
    }
    /// Native zero means unlimited: keep an EOF/overflow probe even at zero budget.
    pub(super) fn probe(&self) -> usize {
        self.options
            .max_entries
            .saturating_sub(self.entries)
            .saturating_add(1)
    }
    pub(super) fn charge(
        &mut self,
        path: Option<&Path>,
        copies: usize,
        index: Option<usize>,
    ) -> Result<(), VfError> {
        let entries = self.entries.checked_add(copies);
        let bytes = path
            .and_then(|p| p.as_os_str().len().checked_mul(copies))
            .and_then(|n| self.bytes.checked_add(n));
        if entries.is_none_or(|n| n > self.options.max_entries)
            || bytes.is_none_or(|n| n > self.options.max_path_bytes)
        {
            return Err(VfError::errno(index, libc::EFBIG as u32));
        }
        self.entries = entries.expect("validated entry budget");
        self.bytes = bytes.expect("validated path budget");
        Ok(())
    }
}

pub(super) fn prepare_listing_entry(
    attrs: &VfAttrs,
    fields: AttrMask,
    index: Option<usize>,
) -> Result<(vfsi_attrs, CString), VfError> {
    if !attrs.returned.contains(fields | AttrMask::MODE) {
        return Err(VfError::unsupported(index));
    }
    let name = attrs
        .file
        .path()
        .and_then(Path::file_name)
        .and_then(|name| cstr_from_os(name.as_bytes()))
        .ok_or_else(|| VfError::invalid_reply(index, "invalid listing entry name"))?;
    Ok((vfsi_attrs::from_vf(attrs), name))
}

/// All collection/validation precedes this step; dropping a stopped iterator
/// also releases any undelivered owned child cursors.
pub(super) fn deliver_listing<R>(
    results: &mut [vfsi_listing_result],
    rows: impl IntoIterator<Item = (usize, R)>,
    mut callback: impl FnMut(usize, R) -> bool,
) {
    for result in results.iter_mut() {
        result.completion = 1;
    }
    for (index, row) in rows {
        results[index].delivered += 1;
        if !callback(index, row) {
            for result in results.iter_mut() {
                result.completion = 2;
            }
            break;
        }
    }
}

/// Complete bounded vector listings, with callbacks outside backend locks.
/// Collect the bounded cohort before delivery, so callers may safely fall back
/// on a listing error without mixing a partial snapshot into ordinary scanning.
/// Budgets include duplicate operands. Results are associated with input indices.
/// A false callback stops delivery successfully and is distinguishable from EOF.
/// A callback stop marks the entire cohort stopped, even if an earlier empty
/// directory had no entries to deliver. Requested but absent attributes fail
/// with ENOTSUP before delivery; applications must not fabricate stat values.
/// Operand planning is additionally capped by the default directory entry/path
/// limits, independently of the configured response quotas.
/// Backend first-page fetches use bounded directory cohorts; aggregate quotas
/// and deferred callback delivery span all cohorts, including duplicate inputs.
///
/// # Safety
/// `fs` must be a live handle. `dirs` and `results` must hold `count` elements,
/// and each directory must be NUL-terminated. `options` must be readable for the
/// call. Callback strings/attributes are borrowed only during the invocation.
/// The callback must not free `fs` or mutate the input/result arrays.
#[no_mangle]
pub unsafe extern "C" fn vfsi_listdirs(
    fs: *mut vfsi_fs,
    dirs: *const *const c_char,
    count: usize,
    options: *const vfsi_listing_options,
    cb: vfsi_indexed_listdir_cb,
    userdata: *mut c_void,
    results: *mut vfsi_listing_result,
) -> c_int {
    let Some(cb) = cb else { return libc::EINVAL };
    // SAFETY: forwards the documented caller-owned pointer contract.
    unsafe {
        listdirs_snapshot(
            fs,
            dirs,
            count,
            options,
            false,
            |i, dir, name, attrs, _| cb(i, dir, name, attrs, userdata),
            results,
        )
    }
}

/// Bounded listings with attributes and scoped identity from the same READDIR
/// observation. ABI-v3 vfsi_attrs is unchanged. Unsupported identity fails
/// before any delivery; budgets, cancellation and lock behavior match listdirs.
/// No additional per-entry GETATTR pass is performed.
///
/// # Safety
/// Same pointer and lifetime contract as vfsi_listdirs. The identity pointer
/// is borrowed only for the callback invocation; copy it before returning.
#[no_mangle]
pub unsafe extern "C" fn vfsi_listdirs_with_ids(
    fs: *mut vfsi_fs,
    dirs: *const *const c_char,
    count: usize,
    options: *const vfsi_listing_options,
    cb: vfsi_indexed_listdir_ids_cb,
    userdata: *mut c_void,
    results: *mut vfsi_listing_result,
) -> c_int {
    let Some(cb) = cb else { return libc::EINVAL };
    // SAFETY: forwards the documented caller-owned pointer contract.
    unsafe {
        listdirs_snapshot(
            fs,
            dirs,
            count,
            options,
            true,
            |i, dir, name, attrs, id| {
                cb(
                    i,
                    dir,
                    name,
                    attrs,
                    id.map_or(std::ptr::null(), |id| id),
                    userdata,
                )
            },
            results,
        )
    }
}

unsafe fn listdirs_snapshot(
    fs: *mut vfsi_fs,
    dirs: *const *const c_char,
    count: usize,
    options: *const vfsi_listing_options,
    include_identity: bool,
    mut cb: impl FnMut(
        usize,
        *const c_char,
        *const c_char,
        &vfsi_attrs,
        Option<&vfsi_object_id>,
    ) -> bool,
    results: *mut vfsi_listing_result,
) -> c_int {
    ffi_guard!(libc::EIO, {
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let (Some(fs), Some(options)) = (unsafe { fs.as_ref() }, unsafe { options.as_ref() })
        else {
            return libc::EINVAL;
        };
        if count == 0 {
            return 0;
        }
        if dirs.is_null() || results.is_null() {
            return libc::EINVAL;
        }
        // Caller owns count elements in both arrays.
        // SAFETY: the C contract supplies count aligned, live array elements; mutable
        // arrays are exclusively borrowed for this call.
        let results =
            unsafe { initialized_output_slice(results, count, |_| vfsi_listing_result::default()) };
        let Some(fields) = AttrMask::from_bits(options.attributes) else {
            return libc::EINVAL;
        };
        if !mask().contains(fields | AttrMask::MODE) {
            return libc::ENOTSUP;
        }
        let fields = fields
            | if include_identity {
                AttrMask::identity()
            } else {
                AttrMask::empty()
            };
        // SAFETY: the exported pointer contract keeps all inputs live for the call.
        let plan = match unsafe { plan_directories(fs, dirs, count) } {
            Ok(plan) => plan,
            Err(code) => return code,
        };
        let mut index = std::collections::HashMap::<&Path, Vec<usize>>::new();
        for (i, path) in plan.paths.iter().enumerate() {
            index.entry(path.as_path()).or_default().push(i);
        }
        let refs: Vec<&Path> = index.keys().copied().collect();
        let mut rows = Vec::new();
        // Legacy callbacks receive compact projected rows, just like cursor
        // callbacks. Keep scoped IDs separate so ordinary listings pay no
        // per-entry identity-storage cost.
        let mut identities = include_identity.then(Vec::new);
        let mut projection_error = None;
        let mut budget = ListingBudget::new(options);
        let mut overflow = false;
        let result = match lock_or_io(&fs.fs) {
            Ok(mut backend) => {
                // Native engines can retain every root's first page before
                // invoking cb. Bound that work even when output quotas are tiny.
                const DIRECTORY_COHORT: usize = 32;
                let mut result = Ok(());
                for cohort in refs.chunks(DIRECTORY_COHORT) {
                    // Never pass zero: native zero means unlimited enumeration.
                    let probe = budget.probe();
                    result = backend.vlistdirs_impl(
                        cohort,
                        fields | AttrMask::MODE,
                        probe,
                        false,
                        &mut |attrs, dir| {
                            let Some(indices) = index.get(dir) else {
                                overflow = true;
                                return false;
                            };
                            if budget
                                .charge(attrs.file.path(), indices.len(), None)
                                .is_err()
                            {
                                overflow = true;
                                return false;
                            }
                            if projection_error.is_none() {
                                let prepared =
                                    prepare_listing_entry(attrs, fields, None).and_then(|entry| {
                                        if let Some(identities) = &mut identities {
                                            identities.push(vfsi_object_id::from_attrs(
                                                fs.namespace_id,
                                                attrs,
                                            )?);
                                        }
                                        Ok(entry)
                                    });
                                match prepared {
                                    Ok((attrs, name)) => {
                                        rows.push((attrs, name, indices.as_slice()));
                                    }
                                    Err(error) => projection_error = Some(error),
                                }
                            }
                            // Continue read-only collection after a projection
                            // error: backend and quota errors retain precedence.
                            true
                        },
                    );
                    if result.is_err() || overflow {
                        break;
                    }
                }
                result
            }
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            return vf_code(&error);
        }
        if overflow {
            return libc::EFBIG;
        }
        if let Some(error) = projection_error {
            return vf_code(&error);
        }
        // All collection and validation precede callbacks; rows borrow only
        // stable operand-index groups, never backend paths or attributes.
        let deliveries = rows
            .iter()
            .enumerate()
            .flat_map(|(row, (attrs, name, indices))| {
                indices.iter().map(move |&i| (i, (row, attrs, name)))
            });
        deliver_listing(results, deliveries, |i, (row, attrs, name)| {
            cb(
                i,
                plan.originals[i].as_ptr(),
                name.as_ptr(),
                attrs,
                identities.as_ref().map(|ids| &ids[row]),
            )
        });
        0
    })
}

fn bounded_listdir(
    fs: &vfsi_fs,
    path: &Path,
    fields: AttrMask,
    max_entries: usize,
    max_path_bytes: usize,
) -> Result<Vec<VfAttrs>, VfError> {
    let probe = if max_entries == usize::MAX {
        0
    } else {
        max_entries.saturating_add(1)
    };
    collect_bounded_entries(max_entries, max_path_bytes, |cb| {
        lock_or_io(&fs.fs)?.visit_dir_impl(path, fields, probe, cb)
    })
}

pub(super) fn collect_bounded_entries(
    max_entries: usize,
    max_path_bytes: usize,
    visit: impl FnOnce(&mut dyn FnMut(&VfAttrs) -> bool) -> Result<(), VfError>,
) -> Result<Vec<VfAttrs>, VfError> {
    let mut entries = Vec::new();
    let mut bytes = 0usize;
    let mut overflow = false;
    visit(&mut |entry| {
        let next = bytes.checked_add(entry.file.path().map_or(0, |p| p.as_os_str().len()));
        if entries.len() >= max_entries || next.is_none_or(|n| n > max_path_bytes) {
            overflow = true;
            return false;
        }
        bytes = next.unwrap_or_default();
        entries.push(entry.clone());
        true
    })?;
    if overflow {
        return Err(VfError::errno(0, libc::EFBIG as u32));
    }
    Ok(entries)
}

/// Incremental no-follow traversal. Callbacks run outside the filesystem lock.
/// Pruned directories are never listed. Errors can follow delivered events;
/// callers must not replay side-effectful callbacks through another backend.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_walk(
    fs: *mut vfsi_fs,
    root: *const c_char,
    options: *const vfsi_walk_options,
    cb: vfsi_walk_cb,
    userdata: *mut c_void,
) -> c_int {
    ffi_guard!(libc::EIO, {
        let (Some(fs), Some(root), Some(options), Some(cb)) =
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
            // mutable outputs are exclusive.
            (unsafe { fs.as_ref() }, unsafe { cstr_path(root) }, unsafe { options.as_ref() }, cb)
        else {
            return libc::EINVAL;
        };
        let Some(sort_by_name) = parse_c_bool(options.sort_by_name) else {
            return libc::EINVAL;
        };
        let Some(fields) = AttrMask::from_bits(options.attributes) else {
            return libc::EINVAL;
        };
        let fields = fields | AttrMask::MODE;
        if !mask().contains(fields) {
            return libc::ENOTSUP;
        }
        let depth = if options.max_depth == usize::MAX {
            vfsi_sync::DepthLimit::unlimited()
        } else {
            let Some(depth) = vfsi_sync::DepthLimit::new(options.max_depth) else {
                return libc::EINVAL;
            };
            depth
        };
        let result = (|| -> Result<(), VfError> {
            let path = vpath_for(fs, &root)?;
            let mut root_attrs = VfAttrs {
                file: FileOperand::from_os_path(&path),
                masks: fields,
                ..VfAttrs::default()
            };
            vfsi_sync::internal::collect_vec_result(
                lock_or_io(&fs.fs)?.vgetattrs_nofollow_impl(std::slice::from_mut(&mut root_attrs)),
                1,
            )?;
            if !root_attrs.returned.contains(fields) {
                return Err(VfError::errno(0, libc::ENOTSUP as u32));
            }
            let metadata = vfsi_sync::metadata_from_attrs(root_attrs);
            let walk_options = vfsi_sync::ListDirOptions::new()
                .with_recursive(true)
                .with_max_entries(options.max_entries)
                .with_max_path_bytes(options.max_path_bytes)
                .with_max_depth(depth);
            vfsi_sync::walk_events(
                vfsi_sync::DirEntry::new(path, metadata),
                walk_options,
                sort_by_name,
                |path, limits| {
                    let entries = bounded_listdir(
                        fs,
                        path,
                        fields,
                        limits.max_entries_or(vfsi_sync::api::DEFAULT_DIRECTORY_MAX_ENTRIES),
                        limits.max_path_bytes_or(vfsi_sync::api::DEFAULT_DIRECTORY_MAX_PATH_BYTES),
                    )?;
                    entries
                        .into_iter()
                        .enumerate()
                        .map(|(index, attrs)| {
                            if !attrs.returned.contains(fields) {
                                return Err(VfError::errno(index, libc::ENOTSUP as u32));
                            }
                            let path = attrs
                                .file
                                .path()
                                .ok_or_else(|| VfError::errno(0, libc::EIO as u32))?
                                .to_path_buf();
                            Ok(vfsi_sync::DirEntry::new(
                                path,
                                vfsi_sync::metadata_from_attrs(attrs),
                            ))
                        })
                        .collect()
                },
                |event| {
                    let relative = event
                        .entry
                        .path()
                        .strip_prefix(&fs.backend_root)
                        .map_err(|_| VfError::from_kind(0, std::io::ErrorKind::InvalidInput))?;
                    let path = CString::new(fs.mountpoint.join(relative).as_os_str().as_bytes())
                        .map_err(|_| VfError::from_kind(0, std::io::ErrorKind::InvalidInput))?;
                    let attrs = attrs_from_metadata(event.entry.attrs());
                    let kind = match event.kind {
                        vfsi_sync::WalkEventKind::Enter => 0,
                        vfsi_sync::WalkEventKind::Entry => 1,
                        vfsi_sync::WalkEventKind::Leave => 2,
                    };
                    // SAFETY: the caller provides this callback; borrowed strings,
                    // attributes and bytes remain live until it returns.
                    match unsafe { cb(path.as_ptr(), kind, event.depth, &attrs, userdata) } {
                        0 => Ok(vfsi_sync::WalkControl::Continue),
                        1 => Ok(vfsi_sync::WalkControl::SkipSubtree),
                        2 => Ok(vfsi_sync::WalkControl::Stop),
                        _ => Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput)),
                    }
                },
            )?;
            Ok(())
        })();
        result.map_or_else(|error| vf_code(&error), |_| 0)
    })
}

/// List `dir` and call `cb` for each entry. Returning `false` from `cb`
/// stops the listing.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_listdir(
    fs: *mut vfsi_fs,
    dir: *const c_char,
    cb: vfsi_listdir_cb,
    userdata: *mut c_void,
) -> c_int {
    // SAFETY: this forwards the same live pointers, lengths and callback obligations
    // required by the exported C ABI.
    unsafe {
        vfsi_listdir_with_limits(
            fs,
            dir,
            vfsi_sync::DEFAULT_DIRECTORY_MAX_ENTRIES,
            vfsi_sync::DEFAULT_DIRECTORY_MAX_PATH_BYTES,
            cb,
            userdata,
        )
    }
}

/// One bounded directory listing. Zero permits only an empty directory.
/// Callbacks run outside the filesystem lock.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_listdir_with_limits(
    fs: *mut vfsi_fs,
    dir: *const c_char,
    max_entries: usize,
    max_path_bytes: usize,
    cb: vfsi_listdir_cb,
    userdata: *mut c_void,
) -> c_int {
    ffi_guard!(libc::EIO, {
        // SAFETY: non-null C strings are NUL-terminated and readable throughout this call,
        // as required by the exported ABI.
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let (Some(fs), Some(dir)) = (unsafe { fs.as_ref() }, unsafe { cstr_path(dir) }) else {
            return libc::EINVAL;
        };
        let Some(cb) = cb else {
            return libc::EINVAL;
        };
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let dir = vpath_for(fs, &dir)?;
            let entries = bounded_listdir(fs, &dir, mask(), max_entries, max_path_bytes)?;
            if entries.iter().any(|attrs| !attrs.returned.contains(mask())) {
                return Err(VfError::errno(0, libc::ENOTSUP as u32));
            }
            for a in entries {
                let (attrs, name) = prepare_listing_entry(&a, mask(), Some(0))?;
                // SAFETY: callback was provided by C and is called from the same
                // thread while `attrs`/`name` are alive.
                if !unsafe { cb(name.as_ptr(), &attrs, userdata) } {
                    break;
                }
            }
            Ok::<_, VfError>(())
        }));
        match res {
            Ok(Ok(())) => 0,
            Ok(Err(e)) => vf_code(&e),
            Err(_) => libc::EIO,
        }
    })
}

/// List several directories in one vectorized batch, calling `cb` for each
/// entry with the directory the entry came from.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_listdirv(
    fs: *mut vfsi_fs,
    dirs: *const *const c_char,
    count: usize,
    max_entries: usize,
    recursive: bool,
    cb: vfsi_listdirv_cb,
    userdata: *mut c_void,
) -> c_int {
    ffi_guard!(libc::EIO, {
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let (Some(fs), false, false) = (unsafe { fs.as_ref() }, dirs.is_null(), cb.is_none())
        else {
            return libc::EINVAL;
        };
        let Some(cb) = cb else {
            return libc::EINVAL;
        };
        let mut paths = Vec::with_capacity(count);
        for i in 0..count {
            // SAFETY: `dirs` points to `count` NUL-terminated strings.
            let p = unsafe { *dirs.add(i) };
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            let Some(path) = (unsafe { cstr_path(p) }) else {
                return libc::EINVAL;
            };
            paths.push(path);
        }
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let original_paths = paths.clone();
            let vpaths: Result<Vec<PathBuf>, VfError> =
                paths.iter().map(|p| vpath_for(fs, p)).collect();
            let vpaths = vpaths?;
            let refs: Vec<&Path> = vpaths.iter().map(PathBuf::as_path).collect();
            let vpath_index: std::collections::HashMap<PathBuf, usize> = vpaths
                .iter()
                .enumerate()
                .map(|(i, p)| (p.clone(), i))
                .collect();
            let mountpoint = fs.mountpoint.clone();
            let backend_root = fs.backend_root.clone();
            let c_cb = cb;
            let mut rows = Vec::new();
            {
                let mut backend = lock_or_io(&fs.fs)?;
                backend.vlistdirs_impl(
                    &refs,
                    mask(),
                    max_entries,
                    recursive,
                    &mut |attrs, dir| {
                        rows.push((attrs.clone(), dir.to_path_buf()));
                        true
                    },
                )?;
            }
            if rows
                .iter()
                .any(|(attrs, _)| !attrs.returned.contains(mask()))
            {
                return Err(VfError::errno(0, libc::ENOTSUP as u32));
            }
            for (attrs, dir) in rows {
                let kernel_dir = match vpath_index.get(&dir) {
                    Some(&i) => original_paths[i].clone(),
                    None => {
                        let rel = dir
                            .strip_prefix(&backend_root)
                            .map_err(|_| VfError::from_kind(0, std::io::ErrorKind::InvalidInput))?;
                        mountpoint.join(rel)
                    }
                };
                let dir = kernel_dir.as_os_str().as_bytes();
                let dir = cstr_from_os(dir).ok_or_else(|| {
                    VfError::invalid_reply(None, "invalid listing directory name")
                })?;
                let (a, name) = prepare_listing_entry(&attrs, mask(), None)?;
                // SAFETY: `c_cb` was provided by C and is invoked from the same
                // thread while its arguments are alive. No filesystem mutex is held.
                if !unsafe { c_cb(dir.as_ptr(), name.as_ptr(), &a, userdata) } {
                    break;
                }
            }
            Ok::<_, VfError>(())
        }));
        match res {
            Ok(Ok(())) => 0,
            Ok(Err(e)) => vf_code(&e),
            Err(_) => libc::EIO,
        }
    })
}
