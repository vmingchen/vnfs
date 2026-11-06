//! Shared bounded listing policy; execution remains with each native engine.
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
