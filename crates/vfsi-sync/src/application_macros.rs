//! Internal static forwarding templates shared by native and opaque clients.

#[doc(hidden)]
#[macro_export]
macro_rules! __vfsi_file_methods {
    ($file:ty $(, permissions = $chmod:ident)?) => {
        fn path(&self) -> &Path {
            <$file>::path(self)
        }
        fn attrs(&self) -> Result<Attrs> {
            <$file>::attrs(self)
        }
        fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<usize> {
            <$file>::read_at(self, buffer, offset)
        }
        fn write_at(&self, buffer: &[u8], offset: u64) -> Result<usize> {
            <$file>::write_at(self, buffer, offset)
        }
        fn read_native(&mut self, buffer: &mut [u8]) -> Result<usize> {
            <$file>::read_native(self, buffer)
        }
        fn read_to_end_with_limit(&mut self, max_bytes: usize) -> Result<Vec<u8>> {
            <$file>::read_to_end_with_limit(self, max_bytes)
        }
        fn write_native(&mut self, buffer: &[u8]) -> Result<usize> {
            <$file>::write_native(self, buffer)
        }
        fn seek_native(&mut self, position: SeekFrom) -> Result<u64> {
            <$file>::seek_native(self, position)
        }
        fn sync_data(&self) -> Result<()> {
            <$file>::sync_data(self)
        }
        fn sync_all(&self) -> Result<()> {
            <$file>::sync_all(self)
        }
        $(
        fn set_permissions(&self, permissions: $crate::api::Permissions) -> Result<()> {
            <$file>::$chmod(self, permissions)
        }
        )?
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
     $write_receiver:path, $page_capacity:expr, $open_native:expr, $stream_native:expr) => {
        $crate::__vfsi_client_methods!(
            $client, $receiver, $vread_native, $read_receiver, $vwrite_native,
            $vwrite_all_native, $metadata, $write_receiver, $page_capacity,
            $open_native, $stream_native;
            vrename, vsymlink, vreadlink, vhardlink, vstatfs, vsetattrs,
            limits, vopen, vclose, vmkdir, vcopy, capabilities
        );
    };
    ($client:ty, $receiver:path, $vread_native:expr, $read_receiver:path,
     $vwrite_native:expr, $vwrite_all_native:expr, $metadata:expr,
     $write_receiver:path, $page_capacity:expr, $open_native:expr, $stream_native:expr;
     $rename:ident, $symlink:ident, $readlink:ident, $hardlink:ident,
     $statfs:ident, $setattrs:ident, $limits:ident, $open:ident,
     $close:ident, $mkdir:ident, $copy:ident, $capabilities:ident) => {
        fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
            &self,
            pairs: &[(P, Q)],
            options: vfsi_core::api::RenameOptions,
        ) -> Result<()> {
            <$client>::$rename($receiver(self), pairs, options)
        }
        fn vlistdirs<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: vfsi_core::api::ListDirOptions,
            callback: impl FnMut(usize, DirectoryListing) -> Result<std::ops::ControlFlow<()>>,
        ) -> Result<Vec<$crate::TraversalCompletion>> {
            $crate::application::visit_directory_pages(
                paths,
                options,
                self.limits(),
                |paths| ($page_capacity)($receiver(self), paths),
                |path| {
                    let metadata = self.vgetattrs(
                        &[path],
                        vfsi_core::api::AttrsOptions::new()
                            .fields(vfsi_core::api::Attributes::MODE)
                            .follow_symlinks(false),
                    )?;
                    if metadata.len() != 1 {
                        return Err(vfsi_core::api::Error::transport(
                            None,
                            "invalid directory root metadata count",
                        ));
                    }
                    if !metadata[0].is_dir() {
                        return Err(vfsi_core::api::Error::client(0, libc::ENOTDIR as u32)
                            .with_context("visit_dirs", path));
                    }
                    Ok(())
                },
                |paths, cursors, page_size, max_entries| {
                    <$client>::read_dir_pages_with_fields(
                        $receiver(self),
                        paths,
                        options.attributes(),
                        cursors,
                        page_size,
                        max_entries,
                        options.follows_symlinks(),
                    )
                },
                callback,
            )
        }
        fn vstream<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: $crate::StreamOptions,
            mut callback: impl FnMut(usize, u64, &[u8]) -> Result<bool>,
        ) -> Result<Vec<$crate::StreamCompletion>> {
            let mut output = Vec::new();
            for (index, path) in paths.iter().enumerate() {
                let completion =
                    ($stream_native)($receiver(self), path, options, |offset, data| {
                        callback(index, offset, data)
                    })
                    .map_err(|error| $crate::application::vector_index(error, index))?;
                output.push(completion);
                if matches!(completion, $crate::StreamCompletion::Stopped { .. }) {
                    break;
                }
            }
            Ok(output)
        }
        fn capabilities(&self) -> Result<vfsi_core::Capabilities> {
            <$client>::$capabilities($receiver(self))
        }
        fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()> {
            <$client>::$symlink($receiver(self), pairs)
        }
        fn vreadlink<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<std::path::PathBuf>> {
            <$client>::$readlink($receiver(self), paths)
        }
        fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()> {
            <$client>::$hardlink($receiver(self), pairs)
        }
        fn vstatfs<P: vfsi_core::AsTarget<Self::File>>(
            &self,
            targets: &[P],
        ) -> Result<Vec<vfsi_core::FilesystemStats>> {
            <$client>::$statfs($receiver(self), targets)
        }
        fn vsetattrs<P: vfsi_core::AsTarget<Self::File>>(
            &self,
            updates: &[vfsi_core::SetAttrsOp<P>],
        ) -> Result<()> {
            <$client>::$setattrs($receiver(self), updates)
        }
        fn vgetattrs<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: vfsi_core::api::AttrsOptions,
        ) -> Result<Vec<Attrs>> {
            ($metadata)($receiver(self), paths, options)
        }
        fn limits(&self) -> ResourceLimits {
            <$client>::$limits($receiver(self))
        }

        fn vopen(&self, requests: &[OpenOp]) -> Result<Vec<Self::File>> {
            if requests.len() == 1 {
                // Preserve native symlink resolution and independent-handle state.
                return ($open_native)($receiver(self), requests[0].clone()).map(|file| vec![file]);
            }
            <$client>::$open($receiver(self), requests)
        }
        fn vread<'a>(
            &self,
            ops: impl IntoIterator<Item = vfsi_core::api::ReadOp<'a, Self::File>>,
            options: vfsi_core::api::ReadOptions,
        ) -> Result<Vec<vfsi_core::api::ReadResult>> {
            ($vread_native)($read_receiver(self), ops, options)
        }
        fn vwrite<'a>(
            &self,
            requests: &[vfsi_core::api::WriteOp<'a, Self::File>],
            options: vfsi_core::api::WriteOptions,
        ) -> Result<Vec<WriteResult>> {
            let result = if options.writes_all() {
                ($vwrite_all_native)($write_receiver(self), requests)
            } else {
                ($vwrite_native)($write_receiver(self), requests)
            };
            result.map_err($crate::application::public_write_error)
        }
        fn vclose(&self, files: &mut [Self::File]) -> Result<()> {
            <$client>::$close($receiver(self), files)
        }
        fn vmkdir<P: AsRef<Path>>(&self, paths: &[vfsi_core::MkDirOp<P>]) -> Result<()> {
            <$client>::$mkdir($receiver(self), paths)
        }

        fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(
            &self,
            pairs: &[(P, Q)],
            options: vfsi_core::api::CopyOption,
        ) -> Result<()> {
            <$client>::$copy($receiver(self), pairs, options)
        }
        fn vremove<P: AsRef<Path>>(
            &self,
            paths: &[P],
            mode: vfsi_core::api::RemoveMode,
            options: $crate::RemoveOptions,
        ) -> Result<()> {
            match mode {
                vfsi_core::api::RemoveMode::Entry | vfsi_core::api::RemoveMode::Tree => {
                    <$client>::vremove_with_options_native(
                        $receiver(self),
                        paths,
                        mode == vfsi_core::api::RemoveMode::Tree,
                        options,
                    )
                }
                vfsi_core::api::RemoveMode::Contents => {
                    let mut first_error = None;
                    for (index, path) in paths.iter().enumerate() {
                        if let Err(error) = <$client>::remove_dir_contents_with_options(
                            $receiver(self),
                            path,
                            options,
                        ) {
                            let error = $crate::application::vector_index(error, index);
                            if error.is_transport() || !options.continues_on_error() {
                                return Err(error);
                            }
                            first_error.get_or_insert(error);
                        }
                    }
                    first_error.map_or(Ok(()), Err)
                }
            }
        }
    };
}
