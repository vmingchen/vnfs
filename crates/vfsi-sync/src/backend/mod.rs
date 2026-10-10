//! Native synchronous backend contracts.
//! `HandleBackend` supports owned handles; `VectorBackend` adds native vector engines
//! and overridable workflows. Shared algorithms live in [`crate::backend::helpers`].
//!
//! These are execution hooks, not application traits. Use [`crate::Vfsi`] and
//! [`crate::VfsiExt`] on a client for portable filesystem operations.
//! The old root traits and scalar/vector namespace aliases are removed:
//!
//! ```compile_fail,E0432
//! use vfsi_sync::FileSystem;
//! ```
//! ```compile_fail,E0432
//! use vfsi_sync::Backend;
//! ```
//! ```compile_fail,E0432
//! use vfsi_sync::HandleBackend;
//! ```
//! ```compile_fail,E0432
//! use vfsi_sync::VectorBackend;
//! ```
//! ```compile_fail,E0432
//! use vfsi_sync::sfsi;
//! ```
//! ```compile_fail,E0432
//! use vfsi_sync::vfsi;
//! ```
//! ```compile_fail,E0432
//! use vfsi_sync::backend_helpers;
//! ```

/// Shared execution defaults for backend implementers.
#[doc(hidden)]
pub mod helpers;

use crate::*;
use std::path::{Path, PathBuf};
use vfsi_core::internal::ManyResults;

/// Minimum contract for owned handles and descriptor lifecycle.
/// This includes handle-level metadata/statistics hooks, but does not require
/// namespace operations, directory enumeration, or native vector I/O.
pub trait HandleBackend {
    fn vstatfs_impl(&mut self, files: &[VfFile]) -> VfResult<Vec<FilesystemStats>> {
        if files.is_empty() {
            Ok(Vec::new())
        } else {
            Err(VfError::unsupported(0))
        }
    }

    fn close_deferred(&mut self, file: &VfFile) -> VfResult<()> {
        self.close_impl(file)
    }

    fn take_notifications(&mut self) -> Vec<Box<dyn FnOnce() + Send>> {
        Vec::new()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::empty()
    }

    fn abs_path(&self, path: &Path) -> PathBuf {
        path.to_path_buf()
    }

    fn open_path_impl(
        &mut self,
        _base: VfPathBase,
        _pathname: &Path,
        _flags: i32,
        _mode: u32,
    ) -> VfResult<VfFile> {
        Err(VfError::unsupported(0))
    }

    /// One backend synchronization vector. POSIX backends may loop under one
    /// lock because fsync has no vector syscall; protocol backends may batch.
    fn vfsync_impl(&mut self, files: &[VfFile], mode: vfsi_core::api::SyncMode) -> VfRes {
        for (index, file) in files.iter().enumerate() {
            match mode {
                vfsi_core::api::SyncMode::Data => self.sync_data(file),
                vfsi_core::api::SyncMode::All => self.sync_all(file),
            }
            .map_err(|error| error.map_index(|_| index))?;
        }
        Ok(())
    }

    fn sync_data(&mut self, tcf: &VfFile) -> VfResult<()>;

    fn sync_all(&mut self, tcf: &VfFile) -> VfResult<()> {
        self.sync_data(tcf)
    }

    fn chdir(&mut self, _path: &Path) -> VfResult<()> {
        Err(VfError::unsupported(0))
    }

    fn getcwd(&self) -> PathBuf {
        PathBuf::from("/")
    }

    fn seek_raw_impl(&mut self, _tcf: &VfFile, _offset: i64, _whence: SeekFrom) -> VfResult<i64> {
        Err(VfError::unsupported(0))
    }

    fn vf_path(&self, file: &VfFile) -> VfResult<PathBuf> {
        crate::backend::helpers::vf_path_default(self, file)
    }

    fn open_raw_impl(&mut self, pathname: &Path, flags: i32, mode: u32) -> VfResult<VfFile> {
        crate::backend::helpers::open_raw_impl_default(self, pathname, flags, mode)
    }

    fn read_raw_impl(&mut self, file: &VfFile, offset: u64, length: usize) -> VfResult<Vec<u8>> {
        crate::backend::helpers::read_raw_impl_default(self, file, offset, length)
    }

    fn write_raw_impl(&mut self, file: &VfFile, offset: u64, data: &[u8]) -> VfResult<usize> {
        crate::backend::helpers::write_raw_impl_default(self, file, offset, data)
    }

    fn read_file_impl(&mut self, file: &VfFile, max_bytes: usize) -> VfResult<Vec<u8>> {
        crate::backend::helpers::read_file_impl_default(self, file, max_bytes)
    }

    fn open_impl(&mut self, request: &OpenOp) -> VfResult<VfFile>;

    fn close_impl(&mut self, file: &VfFile) -> VfResult<()>;

    fn read_impl(&mut self, request: &ReadOp) -> VfResult<ReadResult>;

    fn read_into_impl(&mut self, request: &ReadOp, buffer: &mut [u8]) -> VfResult<ReadIntoResult> {
        crate::backend::helpers::read_into_impl_default(self, request, buffer)
    }

    fn write_impl(&mut self, request: WriteOp<&VfFile, &[u8]>) -> VfResult<WriteResult>;

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
        crate::backend::helpers::vsetattrs_impl_default(self, updates)
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
pub trait VectorBackend: HandleBackend {
    // Vector I/O and strict opens.
    fn vread_impl(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>>;

    fn vread_into_impl(
        &mut self,
        reads: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> VfResult<Vec<ReadIntoResult>> {
        crate::backend::helpers::vread_into_impl_default(self, reads, buffers)
    }
    /// Execute a borrowed ordered vector. Payloads and target storage remain
    /// with the caller; implementations must not retain them after returning.
    fn vwrite_impl(&mut self, writes: &[WriteOp<&VfFile, &[u8]>]) -> VfResult<Vec<WriteResult>> {
        let _ = writes;
        Err(VfError::unsupported(0))
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
        crate::backend::helpers::vopen_outcomes_impl_default(self, paths, flags, modes)
    }

    fn before_open_cleanup(&mut self, _index: usize, _file: &VfFile) -> VfResult<()> {
        crate::backend::helpers::before_open_cleanup_default(self, _index, _file)
    }

    fn vopen_raw_impl(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<Vec<VfFile>> {
        crate::backend::helpers::vopen_raw_impl_default(self, paths, flags, modes)
    }

    fn vopen_raw_simple_impl(
        &mut self,
        paths: &[&Path],
        flags: i32,
        mode: u32,
    ) -> VfResult<Vec<VfFile>> {
        crate::backend::helpers::vopen_raw_simple_impl_default(self, paths, flags, mode)
    }

    fn vclose_impl(&mut self, files: &[VfFile]) -> VfRes {
        crate::backend::helpers::vclose_impl_default(self, files)
    }

    /// Open every typed request or return an error, cleaning confirmed handles.
    /// Creation and truncation effects are not rolled back.
    fn vopen_impl(&mut self, requests: &[OpenOp]) -> VfResult<Vec<VfFile>> {
        crate::backend::helpers::vopen_typed_default(self, requests)
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
        crate::backend::helpers::stat_impl_default(self, path)
    }

    fn lstat_impl(&mut self, path: &Path) -> VfResult<VfAttrs> {
        crate::backend::helpers::lstat_impl_default(self, path)
    }

    fn fstat_impl(&mut self, tcf: &VfFile) -> VfResult<VfAttrs> {
        crate::backend::helpers::fstat_impl_default(self, tcf)
    }

    fn exists_impl(&mut self, path: &Path) -> VfResult<bool> {
        crate::backend::helpers::exists_impl_default(self, path)
    }

    fn file_type_impl(&mut self, path: &Path) -> VfResult<VfType> {
        crate::backend::helpers::file_type_impl_default(self, path)
    }

    fn metadata_path_impl(&mut self, path: &std::path::Path, follow: bool) -> VfResult<Attrs> {
        crate::backend::helpers::native_metadata_path_impl_default(self, path, follow)
    }

    fn set_metadata_path_impl(&mut self, op: &SetAttrsOp<&std::path::Path>) -> VfResult<()> {
        crate::backend::helpers::native_set_metadata_path_impl_default(self, op)
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

    /// False requires resolving every component without following symlinks;
    /// validate-and-then-follow is not sufficient. Continuations retain the
    /// originally opened directory. Unsupported guarantees must fail closed.
    fn listdir_page_impl(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        max_entries: usize,
        follow_symlinks: bool,
    ) -> VfResult<(Vec<VfAttrs>, Option<DirPageCursor>)> {
        crate::backend::helpers::listdir_page_impl_default(
            self,
            dir,
            masks,
            cursor,
            page_size,
            max_entries,
            follow_symlinks,
        )
    }

    fn directory_page_batch_size(&self) -> usize {
        crate::backend::helpers::directory_page_batch_size_default(self)
    }

    fn vlistdir_pages_impl(
        &mut self,
        dirs: &[&Path],
        masks: AttrMask,
        cursors: Vec<Option<DirPageCursor>>,
        page_size: usize,
        max_entries: usize,
        follow_symlinks: bool,
    ) -> VfResult<Vec<BackendDirectoryPage>> {
        crate::backend::helpers::vlistdir_pages_impl_default(
            self,
            dirs,
            masks,
            cursors,
            page_size,
            max_entries,
            follow_symlinks,
        )
    }

    fn create_dir_impl(&mut self, path: &std::path::Path, mode: u32) -> VfResult<()> {
        crate::backend::helpers::native_create_dir_impl_default(self, path, mode)
    }

    fn read_dir_impl(
        &mut self,
        path: &std::path::Path,
        options: ListDirOptions,
    ) -> VfResult<Vec<DirEntry>> {
        crate::backend::helpers::native_read_dir_impl_default(self, path, options)
    }

    /// Adapt full-metadata paging through the field-aware hook below.
    fn read_dir_page_impl(
        &mut self,
        path: &std::path::Path,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<(Vec<DirEntry>, Option<DirPageCursor>)> {
        crate::backend::helpers::native_read_dir_page_impl_default(
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
        crate::backend::helpers::native_read_dir_page_with_fields_impl_default(
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
        crate::backend::helpers::walk_impl_default(self, root, masks, sort)
    }

    fn walk_with_options_impl(
        &mut self,
        root: &Path,
        masks: AttrMask,
        options: ListDirOptions,
        sort: &mut dyn FnMut(&Path, &mut Vec<VfAttrs>),
    ) -> VfResult<Vec<WalkEntry>> {
        crate::backend::helpers::walk_with_options_impl_default(self, root, masks, options, sort)
    }

    fn vlistdirs_impl(
        &mut self,
        dirs: &[&Path],
        masks: AttrMask,
        max_entries: usize,
        recursive: bool,
        cb: &mut dyn FnMut(&VfAttrs, &Path) -> bool,
    ) -> VfRes {
        crate::backend::helpers::vlistdirs_impl_default(
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
        crate::backend::helpers::visit_dir_impl_default(self, dir, masks, max_entries, cb)
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
        crate::backend::helpers::unlink_impl_default(self, pathname)
    }

    fn vunlink_impl(&mut self, pathnames: &[&Path]) -> VfRes {
        crate::backend::helpers::vunlink_impl_default(self, pathnames)
    }

    fn mkdir_raw_impl(&mut self, path: &Path, mode: u32) -> VfResult<()> {
        crate::backend::helpers::mkdir_raw_impl_default(self, path, mode)
    }

    fn ensure_dir_impl(&mut self, dir: &Path, mode: u32) -> VfResult<()> {
        crate::backend::helpers::ensure_dir_impl_default(self, dir, mode)
    }

    fn remove_impl(&mut self, path: &std::path::Path, recursive: bool) -> VfResult<()> {
        crate::backend::helpers::native_remove_impl_default(self, path, recursive)
    }

    fn remove_dir_contents_impl(&mut self, path: &std::path::Path) -> VfResult<()> {
        crate::backend::helpers::native_remove_dir_contents_impl_default(self, path)
    }

    fn rename_impl(&mut self, from: &std::path::Path, to: &std::path::Path) -> VfResult<()> {
        crate::backend::helpers::native_rename_impl_default(self, from, to)
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
        crate::backend::helpers::symlink_raw_impl_default(self, oldpath, newpath)
    }

    fn readlink_raw_impl(&mut self, path: &Path) -> VfResult<Vec<u8>> {
        crate::backend::helpers::readlink_raw_impl_default(self, path)
    }

    fn symlink_impl(&mut self, target: &std::path::Path, link: &std::path::Path) -> VfResult<()> {
        crate::backend::helpers::native_symlink_impl_default(self, target, link)
    }

    fn hard_link_impl(&mut self, source: &std::path::Path, link: &std::path::Path) -> VfResult<()> {
        crate::backend::helpers::native_hard_link_impl_default(self, source, link)
    }

    fn read_link_impl(&mut self, path: &std::path::Path) -> VfResult<std::path::PathBuf> {
        crate::backend::helpers::native_read_link_impl_default(self, path)
    }

    // Extent and tree copy.
    fn vcopy_data_impl(&mut self, pairs: &[ExtentPair]) -> VfRes {
        let _ = (pairs,);
        Err(VfError::unsupported(0))
    }

    fn vcopy_impl(&mut self, pairs: &[ExtentPair], options: CopyOption) -> VfRes {
        crate::backend::helpers::vcopy_impl_default(self, pairs, options)
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
        crate::backend::helpers::vread_all_impl_default(self, files)
    }

    fn vread_all_with_options_impl(
        &mut self,
        files: &[VfFile],
        options: ReadAllOptions,
    ) -> VfResult<Vec<Vec<u8>>> {
        crate::backend::helpers::vread_all_with_options_impl_default(self, files, options)
    }

    fn vstream_impl(
        &mut self,
        files: &[VfFile],
        chunk_size: usize,
        memory_limit: usize,
        cb: &mut ReadStreamCallback<'_>,
    ) -> VfRes {
        crate::backend::helpers::vstream_impl_default(self, files, chunk_size, memory_limit, cb)
    }

    // Recursive removal and retained directory lifecycle.
    fn before_remove_type(&mut self, _index: usize) -> VfResult<()> {
        crate::backend::helpers::before_remove_type_default(self, _index)
    }

    fn remove_paths_impl(&mut self, objs: &[&Path], recursive: bool) -> VfRes {
        crate::backend::helpers::remove_paths_impl_default(self, objs, recursive)
    }

    fn remove_paths_with_options_impl(
        &mut self,
        objs: &[&Path],
        recursive: bool,
        options: RemoveOptions,
    ) -> VfRes {
        crate::backend::helpers::remove_paths_with_options_impl_default(
            self, objs, recursive, options,
        )
    }

    fn open_dir_impl(&mut self, path: &Path) -> VfResult<VfDir> {
        crate::backend::helpers::open_dir_impl_default(self, path)
    }

    fn remove_dir_contents_handle_impl(&mut self, dir: &VfDir) -> VfRes {
        crate::backend::helpers::remove_dir_contents_handle_impl_default(self, dir)
    }

    fn remove_dir_contents_handle_with_options_impl(
        &mut self,
        dir: &VfDir,
        options: RemoveOptions,
    ) -> VfRes {
        crate::backend::helpers::remove_dir_contents_handle_with_options_impl_default(
            self, dir, options,
        )
    }

    fn close_dir_impl(&mut self, _dir: &VfDir) -> VfResult<()> {
        crate::backend::helpers::close_dir_impl_default(self, _dir)
    }

    fn remove_dir_contents_path_impl(&mut self, dir: &Path) -> VfRes {
        crate::backend::helpers::remove_dir_contents_path_impl_default(self, dir)
    }

    fn remove_dir_contents_path_with_options_impl(
        &mut self,
        dir: &Path,
        options: RemoveOptions,
    ) -> VfRes {
        crate::backend::helpers::remove_dir_contents_path_with_options_impl_default(
            self, dir, options,
        )
    }

    fn ensure_empty_dir_impl(&mut self, dir: &Path) -> VfRes {
        crate::backend::helpers::ensure_empty_dir_impl_default(self, dir)
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
            vfsi_core::open_flags_to_libc(request.flags()).map_err(|error| error.with_index(index))
        })
        .collect()
}
