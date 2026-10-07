//! Native synchronous backend contracts.
//! `FileSystem` supports owned handles; `Backend` adds native vector engines
//! and overridable workflows. Shared algorithms live in `backend_helpers`.
use crate::*;
use std::path::{Path, PathBuf};
use vfsi_core::internal::ManyResults;

/// Minimum contract for owned handles and descriptor lifecycle.
/// This includes handle-level metadata/statistics hooks, but does not require
/// namespace operations, directory enumeration, or native vector I/O.
pub trait FileSystem {
    fn vstatfs_impl(&mut self, files: &[VfFile]) -> VfResult<Vec<FilesystemStats>> {
        crate::backend_helpers::vstatfs_impl_default(self, files)
    }

    fn close_deferred(&mut self, file: &VfFile) -> VfResult<()> {
        crate::backend_helpers::close_deferred_default(self, file)
    }

    fn take_notifications(&mut self) -> Vec<Box<dyn FnOnce() + Send>> {
        crate::backend_helpers::take_notifications_default(self)
    }

    fn capability_bits(&self) -> u64 {
        crate::backend_helpers::capability_bits_default(self)
    }

    fn capabilities(&self) -> Capabilities {
        crate::backend_helpers::typed_capabilities_default(self)
    }

    fn abs_path(&self, path: &Path) -> PathBuf {
        crate::backend_helpers::abs_path_default(self, path)
    }

    fn open_path_impl(
        &mut self,
        base: VfPathBase,
        pathname: &Path,
        flags: i32,
        mode: u32,
    ) -> VfResult<VfFile> {
        crate::backend_helpers::open_path_impl_default(self, base, pathname, flags, mode)
    }

    fn sync_data(&mut self, tcf: &VfFile) -> VfResult<()>;

    fn sync_all(&mut self, tcf: &VfFile) -> VfResult<()> {
        crate::backend_helpers::sync_all_default(self, tcf)
    }

    fn chdir(&mut self, path: &Path) -> VfResult<()> {
        crate::backend_helpers::chdir_default(self, path)
    }

    fn getcwd(&self) -> PathBuf {
        crate::backend_helpers::getcwd_default(self)
    }

    fn seek_raw_impl(&mut self, tcf: &VfFile, offset: i64, whence: SeekFrom) -> VfResult<i64> {
        crate::backend_helpers::seek_raw_impl_default(self, tcf, offset, whence)
    }

    fn vf_path(&self, file: &VfFile) -> VfResult<PathBuf> {
        crate::backend_helpers::vf_path_default(self, file)
    }

    fn open_raw_impl(&mut self, pathname: &Path, flags: i32, mode: u32) -> VfResult<VfFile> {
        crate::backend_helpers::open_raw_impl_default(self, pathname, flags, mode)
    }

    fn read_raw_impl(&mut self, file: &VfFile, offset: u64, length: usize) -> VfResult<Vec<u8>> {
        crate::backend_helpers::read_raw_impl_default(self, file, offset, length)
    }

    fn write_raw_impl(&mut self, file: &VfFile, offset: u64, data: &[u8]) -> VfResult<usize> {
        crate::backend_helpers::write_raw_impl_default(self, file, offset, data)
    }

    fn read_file_impl(&mut self, file: &VfFile, max_bytes: usize) -> VfResult<Vec<u8>> {
        crate::backend_helpers::read_file_impl_default(self, file, max_bytes)
    }

    fn open_impl(&mut self, request: &OpenOp) -> VfResult<VfFile>;

    fn close_impl(&mut self, file: &VfFile) -> VfResult<()>;

    fn read_impl(&mut self, request: &ReadOp) -> VfResult<ReadResult>;

    fn read_into_impl(&mut self, request: &ReadOp, buffer: &mut [u8]) -> VfResult<ReadIntoResult> {
        crate::backend_helpers::read_into_impl_default(self, request, buffer)
    }

    fn write_impl(&mut self, request: WriteOpRef<'_>) -> VfResult<WriteResult>;

    fn seek_impl(&mut self, file: &VfFile, position: std::io::SeekFrom) -> VfResult<u64>;

    /// Query a raw backend target with the same field/symlink options used by
    /// the application interface. Returned attributes retain target identity.
    fn metadata_impl(
        &mut self,
        target: Target<'_, VfFile>,
        options: vfsi_core::api::AttrsOptions,
    ) -> VfResult<VfAttrs>;

    /// Apply one shared attribute operation. Raw timestamps and masks are
    /// converted by the backend, not exposed through a second mutation type.
    fn set_attributes_impl(&mut self, update: &SetAttrsOp<Target<'_, VfFile>>) -> VfResult<()>;

    /// Borrow an ordered batch without transferring request ownership.
    /// Each operation carries its own final-symlink policy; native backends
    /// batch contiguous equal-policy runs and validate all inputs before I/O.
    fn vsetattrs_impl(&mut self, updates: &[SetAttrsOp<Target<'_, VfFile>>]) -> VfResult<()> {
        crate::backend_helpers::vsetattrs_impl_default(self, updates)
    }
}

/// Native vector engines and backend workflow overrides.
///
/// Vector replies preserve input order and cardinality. Indexed failures may
/// follow partial effects; mutations do not promise rollback or safe replay.
/// Optional operations report `Unsupported` and capability bits conservatively.
/// Default workflows call shared helpers; native overrides retain batching,
/// retained identity, resource limits, and protocol recovery behavior.
///
/// This trait is object-safe for C/Python dispatch. No blanket implementation
/// synthesizes a complete backend from scalar methods.
pub trait Backend: FileSystem {
    // Vector I/O and strict opens.
    fn vread_impl(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>>;

    fn vread_into_impl(
        &mut self,
        reads: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> VfResult<Vec<ReadIntoResult>> {
        crate::backend_helpers::vread_into_impl_default(self, reads, buffers)
    }

    fn vwrite_owned_impl(&mut self, writes: &[WriteOp]) -> VfResult<Vec<WriteResult>> {
        let _ = writes;
        Err(VfError::unsupported(0))
    }

    fn vwrite_impl(&mut self, writes: &[WriteOpRef<'_>]) -> VfResult<Vec<WriteResult>> {
        crate::backend_helpers::vwrite_impl_default(self, writes)
    }

    /// Indexed partial outcomes for strict-open collection and cleanup.
    /// This internal ownership seam is distinct from the strict typed boundary.
    #[doc(hidden)]
    fn vopen_outcomes_impl(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<ManyResults<VfFile>> {
        crate::backend_helpers::vopen_outcomes_impl_default(self, paths, flags, modes)
    }

    fn before_open_cleanup(&mut self, _index: usize, _file: &VfFile) -> VfResult<()> {
        crate::backend_helpers::before_open_cleanup_default(self, _index, _file)
    }

    fn vopen_raw_impl(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<Vec<VfFile>> {
        crate::backend_helpers::vopen_raw_impl_default(self, paths, flags, modes)
    }

    fn vopen_raw_simple_impl(
        &mut self,
        paths: &[&Path],
        flags: i32,
        mode: u32,
    ) -> VfResult<Vec<VfFile>> {
        crate::backend_helpers::vopen_raw_simple_impl_default(self, paths, flags, mode)
    }

    fn vclose_impl(&mut self, files: &[VfFile]) -> VfRes {
        crate::backend_helpers::vclose_impl_default(self, files)
    }

    /// Open every typed request or return an error, cleaning confirmed handles.
    /// Creation and truncation effects are not rolled back.
    fn vopen_impl(&mut self, requests: &[OpenOp]) -> VfResult<Vec<VfFile>> {
        crate::backend_helpers::vopen_typed_default(self, requests)
    }

    // Path metadata and vector attribute engines.
    fn vgetattrs_impl(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        let _ = (attrs,);
        Err(VfError::unsupported(0))
    }

    fn vgetattrs_nofollow_impl(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        let _ = (attrs,);
        Err(VfError::unsupported(0))
    }

    fn vsetattrs_raw_impl(&mut self, attrs: &[VfAttrs]) -> VfRes {
        let _ = (attrs,);
        Err(VfError::unsupported(0))
    }

    fn vsetattrs_raw_nofollow_impl(&mut self, attrs: &[VfAttrs]) -> VfRes {
        let _ = (attrs,);
        Err(VfError::unsupported(0))
    }

    fn stat_impl(&mut self, path: &Path) -> VfResult<VfAttrs> {
        crate::backend_helpers::stat_impl_default(self, path)
    }

    fn lstat_impl(&mut self, path: &Path) -> VfResult<VfAttrs> {
        crate::backend_helpers::lstat_impl_default(self, path)
    }

    fn fstat_impl(&mut self, tcf: &VfFile) -> VfResult<VfAttrs> {
        crate::backend_helpers::fstat_impl_default(self, tcf)
    }

    fn exists_impl(&mut self, path: &Path) -> VfResult<bool> {
        crate::backend_helpers::exists_impl_default(self, path)
    }

    fn file_type_impl(&mut self, path: &Path) -> VfResult<VfType> {
        crate::backend_helpers::file_type_impl_default(self, path)
    }

    fn metadata_path_impl(&mut self, path: &std::path::Path, follow: bool) -> VfResult<Attrs> {
        crate::backend_helpers::native_metadata_path_impl_default(self, path, follow)
    }

    fn set_metadata_path_impl(&mut self, op: &SetAttrsOp<&std::path::Path>) -> VfResult<()> {
        crate::backend_helpers::native_set_metadata_path_impl_default(self, op)
    }

    // Paged directory enumeration.
    fn listdir_impl(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> VfResult<Vec<VfAttrs>> {
        let _ = (dir, masks, max_count, recursive);
        Err(VfError::unsupported(0))
    }

    fn listdir_page_impl(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<(Vec<VfAttrs>, Option<DirPageCursor>)> {
        crate::backend_helpers::listdir_page_impl_default(
            self,
            dir,
            masks,
            cursor,
            page_size,
            max_entries,
        )
    }

    fn directory_page_batch_size(&self) -> usize {
        crate::backend_helpers::directory_page_batch_size_default(self)
    }

    fn vlistdir_pages_impl(
        &mut self,
        dirs: &[&Path],
        masks: AttrMask,
        cursors: Vec<Option<DirPageCursor>>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<Vec<BackendDirectoryPage>> {
        crate::backend_helpers::vlistdir_pages_impl_default(
            self,
            dirs,
            masks,
            cursors,
            page_size,
            max_entries,
        )
    }

    fn create_dir_impl(&mut self, path: &std::path::Path, mode: u32) -> VfResult<()> {
        crate::backend_helpers::native_create_dir_impl_default(self, path, mode)
    }

    fn read_dir_impl(
        &mut self,
        path: &std::path::Path,
        options: ReadDirOptions,
    ) -> VfResult<Vec<DirEntry>> {
        crate::backend_helpers::native_read_dir_impl_default(self, path, options)
    }

    /// Adapt full-metadata paging through the field-aware hook below.
    fn read_dir_page_impl(
        &mut self,
        path: &std::path::Path,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<(Vec<DirEntry>, Option<DirPageCursor>)> {
        crate::backend_helpers::native_read_dir_page_impl_default(
            self,
            path,
            cursor,
            page_size,
            max_entries,
        )
    }

    /// Adapt native attribute pages without materializing a full listing.
    /// Specialized owned-page implementations should override this hook;
    /// `read_dir_page_impl` forwards here with the full metadata mask.
    fn read_dir_page_with_fields_impl(
        &mut self,
        path: &std::path::Path,
        fields: AttrMask,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<(Vec<DirEntry>, Option<DirPageCursor>)> {
        crate::backend_helpers::native_read_dir_page_with_fields_impl_default(
            self,
            path,
            fields,
            cursor,
            page_size,
            max_entries,
        )
    }

    // Traversal workflows; overrides retain native batches and anchored seeds.
    fn walk_impl(
        &mut self,
        root: &Path,
        masks: AttrMask,
        sort: &mut dyn FnMut(&Path, &mut Vec<VfAttrs>),
    ) -> VfResult<Vec<WalkEntry>> {
        crate::backend_helpers::walk_impl_default(self, root, masks, sort)
    }

    fn walk_with_options_impl(
        &mut self,
        root: &Path,
        masks: AttrMask,
        options: WalkOptions,
        sort: &mut dyn FnMut(&Path, &mut Vec<VfAttrs>),
    ) -> VfResult<Vec<WalkEntry>> {
        crate::backend_helpers::walk_with_options_impl_default(self, root, masks, options, sort)
    }

    fn vlistdirs_impl(
        &mut self,
        dirs: &[&Path],
        masks: AttrMask,
        max_entries: usize,
        recursive: bool,
        cb: &mut dyn FnMut(&VfAttrs, &Path) -> bool,
    ) -> VfRes {
        crate::backend_helpers::vlistdirs_impl_default(
            self,
            dirs,
            masks,
            max_entries,
            recursive,
            cb,
        )
    }

    fn visit_dir_impl(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_entries: usize,
        cb: &mut dyn FnMut(&VfAttrs) -> bool,
    ) -> VfRes {
        crate::backend_helpers::visit_dir_impl_default(self, dir, masks, max_entries, cb)
    }

    // Namespace mutations and scalar adapters.
    fn vrename_impl(&mut self, pairs: &[(VfFile, VfFile)]) -> VfRes {
        let _ = (pairs,);
        Err(VfError::unsupported(0))
    }

    /// Native atomic rename policy; never emulate no-replace with an existence check.
    fn vrename_with_options_impl(
        &mut self,
        pairs: &[(VfFile, VfFile)],
        options: vfsi_core::api::RenameOptions,
    ) -> VfRes {
        match options {
            vfsi_core::api::RenameOptions::Replace => self.vrename_impl(pairs),
            vfsi_core::api::RenameOptions::NoReplace | vfsi_core::api::RenameOptions::Exchange
                if pairs.is_empty() =>
            {
                Ok(())
            }
            vfsi_core::api::RenameOptions::NoReplace | vfsi_core::api::RenameOptions::Exchange => {
                Err(VfError::client(0, vfsi_core::VF_ERR_UNSUPPORTED))
            }
        }
    }

    fn vremove_impl(&mut self, files: &[VfFile]) -> VfRes {
        let _ = (files,);
        Err(VfError::unsupported(0))
    }

    fn vmkdir_impl(&mut self, dirs: &[VfAttrs]) -> VfRes {
        let _ = (dirs,);
        Err(VfError::unsupported(0))
    }

    fn unlink_impl(&mut self, pathname: &Path) -> VfResult<()> {
        crate::backend_helpers::unlink_impl_default(self, pathname)
    }

    fn vunlink_impl(&mut self, pathnames: &[&Path]) -> VfRes {
        crate::backend_helpers::vunlink_impl_default(self, pathnames)
    }

    fn mkdir_raw_impl(&mut self, path: &Path, mode: u32) -> VfResult<()> {
        crate::backend_helpers::mkdir_raw_impl_default(self, path, mode)
    }

    fn ensure_dir_impl(&mut self, dir: &Path, mode: u32) -> VfResult<()> {
        crate::backend_helpers::ensure_dir_impl_default(self, dir, mode)
    }

    fn remove_impl(&mut self, path: &std::path::Path, recursive: bool) -> VfResult<()> {
        crate::backend_helpers::native_remove_impl_default(self, path, recursive)
    }

    fn remove_dir_contents_impl(&mut self, path: &std::path::Path) -> VfResult<()> {
        crate::backend_helpers::native_remove_dir_contents_impl_default(self, path)
    }

    fn rename_impl(&mut self, from: &std::path::Path, to: &std::path::Path) -> VfResult<()> {
        crate::backend_helpers::native_rename_impl_default(self, from, to)
    }

    // Links.
    fn vsymlink_impl(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
        let _ = (oldpaths, newpaths);
        Err(VfError::unsupported(0))
    }

    fn vreadlink_impl(&mut self, paths: &[&Path]) -> VfResult<Vec<Vec<u8>>> {
        let _ = (paths,);
        Err(VfError::unsupported(0))
    }

    fn vhardlink_impl(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
        let _ = (oldpaths, newpaths);
        Err(VfError::unsupported(0))
    }

    fn symlink_raw_impl(&mut self, oldpath: &Path, newpath: &Path) -> VfResult<()> {
        crate::backend_helpers::symlink_raw_impl_default(self, oldpath, newpath)
    }

    fn readlink_raw_impl(&mut self, path: &Path) -> VfResult<Vec<u8>> {
        crate::backend_helpers::readlink_raw_impl_default(self, path)
    }

    fn symlink_impl(&mut self, target: &std::path::Path, link: &std::path::Path) -> VfResult<()> {
        crate::backend_helpers::native_symlink_impl_default(self, target, link)
    }

    fn hard_link_impl(&mut self, source: &std::path::Path, link: &std::path::Path) -> VfResult<()> {
        crate::backend_helpers::native_hard_link_impl_default(self, source, link)
    }

    fn read_link_impl(&mut self, path: &std::path::Path) -> VfResult<std::path::PathBuf> {
        crate::backend_helpers::native_read_link_impl_default(self, path)
    }

    // Extent and tree copy.
    fn vcopy_data_impl(&mut self, pairs: &[ExtentPair]) -> VfRes {
        let _ = (pairs,);
        Err(VfError::unsupported(0))
    }

    fn vcopy_impl(&mut self, pairs: &[ExtentPair], options: CopyOption) -> VfRes {
        crate::backend_helpers::vcopy_impl_default(self, pairs, options)
    }

    fn copy_tree_impl(
        &mut self,
        src_dir: &Path,
        dst: &Path,
        symlinks: bool,
        use_server_side_copy: bool,
    ) -> VfRes {
        let _ = (src_dir, dst, symlinks, use_server_side_copy);
        Err(VfError::unsupported(0))
    }

    // Bounded whole-file reads and streams.
    fn vread_all_impl(&mut self, files: &[VfFile]) -> VfResult<Vec<Vec<u8>>> {
        crate::backend_helpers::vread_all_impl_default(self, files)
    }

    fn vread_all_with_options_impl(
        &mut self,
        files: &[VfFile],
        options: ReadAllOptions,
    ) -> VfResult<Vec<Vec<u8>>> {
        crate::backend_helpers::vread_all_with_options_impl_default(self, files, options)
    }

    fn vstream_impl(
        &mut self,
        files: &[VfFile],
        chunk_size: usize,
        memory_limit: usize,
        cb: &mut ReadStreamCallback<'_>,
    ) -> VfRes {
        crate::backend_helpers::vstream_impl_default(self, files, chunk_size, memory_limit, cb)
    }

    // Recursive removal and retained directory lifecycle.
    fn before_remove_type(&mut self, _index: usize) -> VfResult<()> {
        crate::backend_helpers::before_remove_type_default(self, _index)
    }

    fn remove_paths_impl(&mut self, objs: &[&Path], recursive: bool) -> VfRes {
        crate::backend_helpers::remove_paths_impl_default(self, objs, recursive)
    }

    fn remove_paths_with_options_impl(
        &mut self,
        objs: &[&Path],
        recursive: bool,
        options: RemoveOptions,
    ) -> VfRes {
        crate::backend_helpers::remove_paths_with_options_impl_default(
            self, objs, recursive, options,
        )
    }

    fn open_dir_impl(&mut self, path: &Path) -> VfResult<VfDir> {
        crate::backend_helpers::open_dir_impl_default(self, path)
    }

    fn remove_dir_contents_handle_impl(&mut self, dir: &VfDir) -> VfRes {
        crate::backend_helpers::remove_dir_contents_handle_impl_default(self, dir)
    }

    fn remove_dir_contents_handle_with_options_impl(
        &mut self,
        dir: &VfDir,
        options: RemoveOptions,
    ) -> VfRes {
        crate::backend_helpers::remove_dir_contents_handle_with_options_impl_default(
            self, dir, options,
        )
    }

    fn close_dir_impl(&mut self, _dir: &VfDir) -> VfResult<()> {
        crate::backend_helpers::close_dir_impl_default(self, _dir)
    }

    fn remove_dir_contents_path_impl(&mut self, dir: &Path) -> VfRes {
        crate::backend_helpers::remove_dir_contents_path_impl_default(self, dir)
    }

    fn remove_dir_contents_path_with_options_impl(
        &mut self,
        dir: &Path,
        options: RemoveOptions,
    ) -> VfRes {
        crate::backend_helpers::remove_dir_contents_path_with_options_impl_default(
            self, dir, options,
        )
    }

    fn ensure_empty_dir_impl(&mut self, dir: &Path) -> VfRes {
        crate::backend_helpers::ensure_empty_dir_impl_default(self, dir)
    }

    // Optional application-data-block writes.
    fn vwrite_adb_impl(&mut self, patterns: &[Adb]) -> VfResult<Vec<usize>> {
        let _ = (patterns,);
        Err(VfError::unsupported(0))
    }
}
pub(crate) fn metadata_mask() -> AttrMask {
    AttrMask::MODE
        | AttrMask::SIZE
        | AttrMask::NLINK
        | AttrMask::FILEID
        | AttrMask::UID
        | AttrMask::GID
        | AttrMask::ATIME
        | AttrMask::MTIME
        | AttrMask::CTIME
        | AttrMask::CHANGE
}

pub(crate) fn system_time_parts(time: std::time::SystemTime) -> VfResult<(i64, u32)> {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => Ok((
            i64::try_from(duration.as_secs())
                .map_err(|_| VfError::client(0, libc::EOVERFLOW as u32))?,
            duration.subsec_nanos(),
        )),
        Err(error) => {
            let duration = error.duration();
            let seconds = i64::try_from(duration.as_secs())
                .map_err(|_| VfError::client(0, libc::EOVERFLOW as u32))?;
            if duration.subsec_nanos() == 0 {
                Ok((-seconds, 0))
            } else {
                let seconds = seconds
                    .checked_add(1)
                    .ok_or_else(|| VfError::client(0, libc::EOVERFLOW as u32))?;
                Ok((-seconds, 1_000_000_000 - duration.subsec_nanos()))
            }
        }
    }
}
#[cfg(unix)]
pub(crate) fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes).into()
}
#[cfg(not(unix))]
pub(crate) fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    String::from_utf8_lossy(&bytes).into_owned().into()
}
pub(crate) fn translate_open_flags(requests: &[OpenOp]) -> VfResult<Vec<i32>> {
    requests
        .iter()
        .enumerate()
        .map(|(index, request)| {
            vfsi_core::open_flags_to_libc(request.flags).map_err(|error| error.with_index(index))
        })
        .collect()
}
