//! Internal static forwarding templates shared by native and opaque clients.

#[doc(hidden)]
#[macro_export]
macro_rules! __vfsi_file_methods {
    ($file:ty) => {
        fn path(&self) -> &Path {
            <$file>::path(self)
        }
        fn try_close(&mut self) -> Result<()> {
            <$file>::try_close(self)
        }
        fn is_closed(&self) -> bool {
            <$file>::is_closed(self)
        }
        fn close(self) -> Result<()> {
            <$file>::close(self)
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __vfsi_client_methods {
    ($client:ty, $receiver:path, $vread_native:expr, $read_receiver:path,
     $vwrite_native:expr, $vwrite_all_native:expr, $metadata:expr,
     $write_receiver:path, $page_capacity:expr) => {
        $crate::__vfsi_client_methods!(
            $client, $receiver, $vread_native, $read_receiver, $vwrite_native,
            $vwrite_all_native, $metadata, $write_receiver, $page_capacity;
            vrename, vsymlink, vreadlink, vhardlink, vstatfs, vsetattrs,
            limits, vopen, vclose, vmkdir, vcopy, capabilities
        );
    };
    ($client:ty, $receiver:path, $vread_native:expr, $read_receiver:path,
     $vwrite_native:expr, $vwrite_all_native:expr, $metadata:expr,
     $write_receiver:path, $page_capacity:expr;
     $rename:ident, $symlink:ident, $readlink:ident, $hardlink:ident,
     $statfs:ident, $setattrs:ident, $limits:ident, $open:ident,
     $close:ident, $mkdir:ident, $copy:ident, $capabilities:ident) => {
        fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
            &self,
            pairs: &[(P, Q)],
            options: vfsi_core::api::RenameMode,
        ) -> vfsi_core::VecResult<()> {
            <$client>::$rename($receiver(self), pairs, options)
        }
        fn vlistdir_pages<P: AsRef<Path>>(
            &self,
            paths: &[P],
            cursors: Vec<Option<vfsi_core::api::DirectoryCursor>>,
            options: vfsi_core::api::ListDirOptions,
            page_size: std::num::NonZeroUsize,
        ) -> vfsi_core::VecResult<vfsi_core::api::DirectoryPage> {
            <$client>::vlistdir_pages_impl($receiver(self), paths, cursors, options, page_size)
        }
        fn vlistdirs<P: AsRef<Path>, C: Into<vfsi_core::api::ListDirControl>>(
            &self,
            paths: &[P],
            options: vfsi_core::api::ListDirOptions,
            callback: impl FnMut(usize, DirectoryListing) -> Result<C>,
        ) -> vfsi_core::VecResult<$crate::TraversalCompletion> {
            $crate::application::visit_directory_pages(
                paths,
                options,
                self.limits(),
                |paths| ($page_capacity)($receiver(self), paths),
                |path| {
                    let mut metadata = vfsi_core::internal::vec_result_slots(
                        vfsi_core::api::Vfsi::vgetattrs(self, &[path],
                            vfsi_core::api::AttrsOptions::new()
                                .with_fields(vfsi_core::api::Attributes::MODE)
                                .with_follow_final_symlink(false)),
                        1,
                    );
                    metadata.remove(0).and_then(|metadata| {
                        if metadata.is_dir() { Ok(()) } else {
                            Err(vfsi_core::OpError::failed(
                                vfsi_core::api::Error::errno(None, libc::ENOTDIR as u32)
                                    .with_context("visit_dirs", path), None))
                        }
                    })
                },
                |paths, cursors, page_size, max_entries, follow_symlinks| {
                    <$client>::read_dir_pages_with_fields(
                        $receiver(self),
                        paths,
                        options.fields(),
                        cursors,
                        page_size,
                        max_entries,
                        follow_symlinks,
                    )
                },
                callback,
            )
        }
        fn vstream<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: $crate::StreamOptions,
            callback: impl FnMut(usize, u64, &[u8]) -> Result<std::ops::ControlFlow<()>>,
        ) -> vfsi_core::VecResult<$crate::StreamCompletion> {
            $crate::application::stream_cohorts(self, paths, options, callback)
        }
        fn capabilities(&self) -> Result<vfsi_core::Capabilities> {
            <$client>::$capabilities($receiver(self))
        }
        fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> vfsi_core::VecResult<()> {
            <$client>::$symlink($receiver(self), pairs)
        }
        fn vreadlink<P: AsRef<Path>>(&self, paths: &[P]) -> vfsi_core::VecResult<std::path::PathBuf> {
            <$client>::$readlink($receiver(self), paths)
        }
        fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> vfsi_core::VecResult<()> {
            <$client>::$hardlink($receiver(self), pairs)
        }
        fn vstatfs<P: vfsi_core::AsTarget<Self::File>>(
            &self,
            targets: &[P],
        ) -> vfsi_core::VecResult<vfsi_core::FilesystemStats> {
            <$client>::$statfs($receiver(self), targets)
        }
        fn vsetattrs<P: vfsi_core::AsTarget<Self::File>>(
            &self,
            updates: &[vfsi_core::SetAttrsOp<P>],
        ) -> vfsi_core::VecResult<()> {
            <$client>::$setattrs($receiver(self), updates)
        }
        fn vfsync(&self, files: &[&Self::File], mode: vfsi_core::api::SyncMode) -> vfsi_core::VecResult<()> {
            <$client>::vfsync_impl($receiver(self), files, mode)
        }
        fn vgetattrs<P: vfsi_core::AsTarget<Self::File>>(
            &self,
            paths: &[P],
            options: vfsi_core::api::AttrsOptions,
        ) -> vfsi_core::VecResult<Attrs> {
            ($metadata)($receiver(self), paths, options)
        }
        fn limits(&self) -> ResourceLimits {
            <$client>::$limits($receiver(self))
        }

        fn vopen_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> vfsi_core::VecResult<Self::Dir> {
            <$client>::vopen_dirs_impl($receiver(self), paths)
        }
        fn vremove_dir_contents(&self, dirs: &[&Self::Dir], options: $crate::RemoveOptions) -> vfsi_core::VecResult<()> {
            <$client>::vremove_dir_contents_impl($receiver(self), dirs, options)
        }
        fn vopen(&self, requests: &[OpenOp]) -> vfsi_core::VecResult<Self::File> {
            <$client>::$open($receiver(self), requests)
        }
        fn vread<'a>(
            &self,
            ops: impl IntoIterator<Item = vfsi_core::api::ReadOp<'a, Self::File>>,
            options: vfsi_core::api::ReadOptions,
        ) -> vfsi_core::VecResult<vfsi_core::api::ReadResult> {
            ($vread_native)($read_receiver(self), ops, options)
        }
        fn vwrite<'a>(
            &self,
            requests: &[vfsi_core::api::WriteOp<'a, Self::File>],
            options: vfsi_core::api::WriteOptions,
        ) -> vfsi_core::VecResult<WriteResult> {
            let result = if options.write_all() {
                ($vwrite_all_native)($write_receiver(self), requests)
            } else {
                ($vwrite_native)($write_receiver(self), requests)
            };
            $crate::application::public_write_results(result)
        }
        fn vclose(&self, files: &mut [Self::File]) -> vfsi_core::VecResult<()> {
            <$client>::$close($receiver(self), files)
        }
        fn vmkdir<P: AsRef<Path>>(&self, paths: &[vfsi_core::MkDirOp<P>]) -> vfsi_core::VecResult<()> {
            <$client>::$mkdir($receiver(self), paths)
        }

        fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(
            &self,
            pairs: &[(P, Q)],
            options: vfsi_core::api::CopyOptions,
        ) -> vfsi_core::VecResult<()> {
            <$client>::$copy($receiver(self), pairs, options)
        }
        fn vremove<P: AsRef<Path>>(
            &self, paths: &[P], mode: vfsi_core::api::RemoveMode, options: $crate::RemoveOptions,
        ) -> vfsi_core::VecResult<()> {
            <$client>::vremove_impl($receiver(self),paths,mode,options)
        }
    };
}
