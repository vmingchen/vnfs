//! Concrete client implementations of the portable VFSI contracts.
#[cfg(test)]
use crate::VfsiExt;
use crate::{
    Attrs, DirectoryListing, FileHandle, OpenOp, ResourceLimits, Result, Vfsi, WriteResult,
};
use std::io::SeekFrom;
use std::path::Path;
use vfsi_sync::application::visit_directory_pages;
fn vector_index(error: crate::Error, index: usize) -> crate::Error {
    if error.index().is_some() {
        error.with_index(index)
    } else {
        error
    }
}
macro_rules! file_methods {
    ($file:ty) => {
        fn path(&self) -> &Path {
            <$file>::path(self)
        }
        fn attrs(&self) -> Result<Attrs> {
            <$file>::attrs(self)
        }
        fn read_request_at(&self, offset: u64, length: usize) -> Self::ReadRequest<'_> {
            <$file>::read_request_at(self, offset, length)
        }
        fn read_request_at_into<'a>(
            &'a self,
            offset: u64,
            buffer: &'a mut [u8],
        ) -> Self::ReadIntoRequest<'a> {
            <$file>::read_request_at_into(self, offset, buffer)
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
        fn set_permissions(&self, permissions: crate::Permissions) -> Result<()> {
            <$file>::chmod(self, permissions)
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

pub(crate) trait NativeHooks: Vfsi {
    fn page_capacity(&self, paths: &[&Path]) -> Result<usize>;
    fn open_native(&self, request: OpenOp) -> Result<Self::File>;
    fn stream_native(
        &self,
        path: impl AsRef<Path>,
        options: crate::StreamOptions,
        callback: impl FnMut(u64, &[u8]) -> Result<bool>,
    ) -> Result<crate::StreamCompletion>;
}

macro_rules! client_methods {
    ($client:ty, $receiver:path) => {
        client_methods!($client, $receiver, <$client>::vread);
    };
    ($client:ty, $receiver:path, $vread_native:expr) => {
        client_methods!($client, $receiver, $vread_native, $receiver);
    };
    ($client:ty, $receiver:path, $vread_native:expr, $read_receiver:path) => {
        client_methods!(
            $client,
            $receiver,
            $vread_native,
            $read_receiver,
            <$client>::write_partial_native,
            <$client>::write_complete
        );
    };
    ($client:ty, $receiver:path, $vread_native:expr, $read_receiver:path, $vwrite_native:expr, $vwrite_all_native:expr) => {
        client_methods!(
            $client,
            $receiver,
            $vread_native,
            $read_receiver,
            $vwrite_native,
            $vwrite_all_native,
            <$client>::vgetattrs
        );
    };
    ($client:ty, $receiver:path, $vread_native:expr, $read_receiver:path, $vwrite_native:expr, $vwrite_all_native:expr, $metadata:expr) => {
        client_methods!(
            $client,
            $receiver,
            $vread_native,
            $read_receiver,
            $vwrite_native,
            $vwrite_all_native,
            $metadata,
            $receiver
        );
    };
    ($client:ty, $receiver:path, $vread_native:expr, $read_receiver:path, $vwrite_native:expr, $vwrite_all_native:expr, $metadata:expr, $write_receiver:path) => {
        client_methods!(
            $client,
            $receiver,
            $vread_native,
            $read_receiver,
            $vwrite_native,
            $vwrite_all_native,
            $metadata,
            $write_receiver,
            vrename,
            vmkdir,
            vcopy,
            vclose,
            vopen
        );
    };
    // Application clients and backend clients use different native method names.
    ($client:ty, $receiver:path, $vread_native:expr, $read_receiver:path, $vwrite_native:expr, $vwrite_all_native:expr, $metadata:expr, $write_receiver:path, $rename:ident, $mkdir:ident, $copy:ident, $close:ident, $open_batch:ident) => {
        fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
            &self,
            pairs: &[(P, Q)],
            options: crate::RenameOptions,
        ) -> Result<()> {
            <$client>::vrename($receiver(self), pairs, options)
        }
        fn vlistdirs<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: crate::ListDirOptions,
            callback: impl FnMut(usize, DirectoryListing) -> Result<std::ops::ControlFlow<()>>,
        ) -> Result<Vec<crate::TraversalCompletion>> {
            visit_directory_pages(
                paths,
                options,
                self.limits(),
                |paths| <$client as NativeHooks>::page_capacity($receiver(self), paths),
                |path| {
                    let metadata = self.vgetattrs(
                        &[path],
                        crate::AttrsOptions::new()
                            .fields(crate::Attributes::MODE)
                            .follow_symlinks(false),
                    )?;
                    if metadata.len() != 1 {
                        return Err(crate::Error::transport(
                            None,
                            "invalid directory root metadata count",
                        ));
                    }
                    if !metadata[0].is_dir() {
                        return Err(crate::Error::client(0, libc::ENOTDIR as u32)
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
            options: crate::StreamOptions,
            mut callback: impl FnMut(usize, u64, &[u8]) -> Result<bool>,
        ) -> Result<Vec<crate::StreamCompletion>> {
            let mut output = Vec::new();
            for (index, path) in paths.iter().enumerate() {
                let completion = <$client as NativeHooks>::stream_native(
                    $receiver(self),
                    path,
                    options,
                    |offset, data| callback(index, offset, data),
                )
                .map_err(|error| vector_index(error, index))?;
                output.push(completion);
                if matches!(completion, crate::StreamCompletion::Stopped { .. }) {
                    break;
                }
            }
            Ok(output)
        }
        fn capabilities(&self) -> Result<vfsi_core::Capabilities> {
            <$client>::capabilities($receiver(self))
        }
        fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()> {
            <$client>::vsymlink($receiver(self), pairs)
        }
        fn vreadlink<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<std::path::PathBuf>> {
            <$client>::vreadlink($receiver(self), paths)
        }
        fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()> {
            <$client>::vhardlink($receiver(self), pairs)
        }
        fn vstatfs<P: vfsi_core::AsTarget<Self::File>>(
            &self,
            targets: &[P],
        ) -> Result<Vec<vfsi_core::FilesystemStats>> {
            <$client>::vstatfs($receiver(self), targets)
        }
        fn vsetattrs<P: vfsi_core::AsTarget<Self::File>>(
            &self,
            updates: &[vfsi_core::SetAttrsOp<P>],
        ) -> Result<()> {
            <$client>::vsetattrs($receiver(self), updates)
        }
        fn vgetattrs<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: crate::AttrsOptions,
        ) -> Result<Vec<Attrs>> {
            ($metadata)($receiver(self), paths, options)
        }
        fn limits(&self) -> ResourceLimits {
            <$client>::limits($receiver(self))
        }

        fn vopen(&self, requests: &[OpenOp]) -> Result<Vec<Self::File>> {
            if requests.len() == 1 {
                // Preserve native symlink resolution and independent-handle state.
                return <$client as NativeHooks>::open_native($receiver(self), requests[0].clone())
                    .map(|file| vec![file]);
            }
            <$client>::$open_batch($receiver(self), requests)
        }
        fn vread<'a>(
            &self,
            ops: impl IntoIterator<Item = crate::ReadOp<'a, Self::File>>,
            options: crate::ReadOptions,
        ) -> Result<Vec<crate::ReadResult>> {
            ($vread_native)($read_receiver(self), ops, options)
        }
        fn vwrite<'a>(
            &self,
            requests: &[crate::WriteOp<'a, Self::File>],
            options: crate::WriteOptions,
        ) -> Result<Vec<WriteResult>> {
            let result = if options.writes_all() {
                ($vwrite_all_native)($write_receiver(self), requests)
            } else {
                ($vwrite_native)($write_receiver(self), requests)
            };
            result.map_err(crate::write::public_write_error)
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
            options: crate::CopyOption,
        ) -> Result<()> {
            <$client>::$copy($receiver(self), pairs, options)
        }
        fn vremove<P: AsRef<Path>>(
            &self,
            paths: &[P],
            mode: crate::RemoveMode,
            options: crate::RemoveOptions,
        ) -> Result<()> {
            match mode {
                crate::RemoveMode::Entry | crate::RemoveMode::Tree => {
                    <$client>::vremove_with_options_native(
                        $receiver(self),
                        paths,
                        mode == crate::RemoveMode::Tree,
                        options,
                    )
                }
                crate::RemoveMode::Contents => {
                    let mut first_error = None;
                    for (index, path) in paths.iter().enumerate() {
                        if let Err(error) = <$client>::remove_dir_contents_with_options(
                            $receiver(self),
                            path,
                            options,
                        ) {
                            let error = vector_index(error, index);
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

#[cfg(feature = "nfs")]
impl FileHandle for crate::NfsFile {
    type ReadRequest<'a> = crate::NfsRead<'a>;
    type ReadIntoRequest<'a> = crate::NfsReadInto<'a>;
    file_methods!(crate::NfsFile);
}
#[cfg(feature = "nfs")]
impl Vfsi for crate::NfsClient {
    type File = crate::NfsFile;
    client_methods!(crate::NfsClient, std::convert::identity);
}

#[cfg(all(feature = "auto", target_os = "linux"))]
mod routed {
    use super::*;
    impl FileHandle for crate::AutoFile {
        type ReadRequest<'a> = crate::AutoRead<'a>;
        type ReadIntoRequest<'a> = crate::AutoReadInto<'a>;
        file_methods!(crate::AutoFile);
    }
    impl Vfsi for crate::AutoClient {
        type File = crate::AutoFile;
        client_methods!(crate::AutoClient, std::convert::identity);
    }

    impl Vfsi for crate::Auto {
        type File = crate::AutoFile;
        client_methods!(crate::AutoClient, std::ops::Deref::deref);
    }

    impl Vfsi for crate::Mounted {
        type File = crate::MountedFile;
        client_methods!(crate::Mounted, std::convert::identity);
    }

    impl FileHandle for crate::MountedFile {
        type ReadRequest<'a> = crate::MountedRead<'a>;
        type ReadIntoRequest<'a> = crate::MountedReadInto<'a>;
        file_methods!(crate::MountedFile);
    }
}

#[cfg(all(test, feature = "auto", target_os = "linux"))]
mod extension_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn page_faults_do_not_replay_transport_failures_and_reject_malformed_shapes() {
        use crate::{ControlFlow, ListDirOptions, ResourceLimits};
        for malformed in 0..4 {
            let mut calls = 0;
            let result = super::visit_directory_pages(
                &["/a"],
                ListDirOptions::new(),
                ResourceLimits::default(),
                |_| Ok(32),
                |_| Ok(()),
                |_, _, _, _| {
                    calls += 1;
                    match malformed {
                        0 => Err(crate::Error::transport(None, "lost reply")),
                        1 => Ok(Vec::new()),
                        2 => Ok(vec![(
                            DirectoryListing {
                                path: "/wrong".into(),
                                entries: Vec::new(),
                            },
                            None,
                            Vec::new(),
                        )]),
                        _ => Ok(vec![(
                            DirectoryListing {
                                path: "/a".into(),
                                entries: Vec::new(),
                            },
                            Some(vfsi_sync::DirPageCursor::new(0usize)),
                            Vec::new(),
                        )]),
                    }
                },
                |_, _| panic!("invalid pages must not be delivered"),
            );
            assert!(result.unwrap_err().is_transport());
            assert_eq!(calls, 1);
        }
        let mut calls = 0;
        let result = super::visit_directory_pages(
            &["/a", "/missing"],
            ListDirOptions::new(),
            ResourceLimits::default(),
            |_| Ok(32),
            |_| Ok(()),
            |paths, _, _, _| {
                calls += 1;
                if paths.len() == 2 {
                    return Err(crate::Error::client(1, libc::ENOENT as u32));
                }
                Ok(vec![(
                    DirectoryListing {
                        path: "/a".into(),
                        entries: Vec::new(),
                    },
                    None,
                    Vec::new(),
                )])
            },
            |index, page| {
                assert_eq!(index, 0);
                assert!(page.entries.is_empty());
                Ok(ControlFlow::Break(()))
            },
        )
        .unwrap();
        assert_eq!(result, [crate::TraversalCompletion::Stopped]);
        assert_eq!(calls, 2, "only a known semantic prefix is re-fetched");
    }

    #[test]
    fn directory_continuations_run_in_vector_waves_and_cancellation_marks_unfinished_roots() {
        use crate::{ControlFlow, ListDirOptions, ResourceLimits};
        let temp = tempfile::tempdir().unwrap();
        let mounted = crate::Mounted::new(temp.path()).unwrap();
        mounted.write("/f", b"x").unwrap();
        let metadata = mounted.attrs("/f").unwrap();
        for stop in [false, true] {
            let mut widths = Vec::new();
            let result = super::visit_directory_pages(
                &["/a", "/b"],
                ListDirOptions::new(),
                ResourceLimits::default(),
                |_| Ok(32),
                |_| Ok(()),
                |paths, cursors, _, _| {
                    widths.push(paths.len());
                    paths
                        .iter()
                        .zip(cursors)
                        .map(|(path, cursor)| {
                            let step = match cursor {
                                Some(cursor) => cursor.into_state::<usize>()?,
                                None => 0,
                            };
                            Ok((
                                DirectoryListing {
                                    path: path.to_path_buf(),
                                    entries: vec![crate::DirEntry::new(
                                        path.join(format!("f{step}")),
                                        metadata.clone(),
                                    )],
                                },
                                (step < 2).then(|| vfsi_sync::DirPageCursor::new(step + 1)),
                                Vec::new(),
                            ))
                        })
                        .collect()
                },
                |index, page| {
                    Ok(
                        if stop && index == 0 && page.entries[0].path().ends_with("f1") {
                            ControlFlow::Break(())
                        } else {
                            ControlFlow::Continue(())
                        },
                    )
                },
            )
            .unwrap();
            if stop {
                assert_eq!(widths, [2, 2]);
                assert_eq!(result, [crate::TraversalCompletion::Stopped; 2]);
            } else {
                assert_eq!(widths, [2, 2, 2]);
                assert_eq!(result, [crate::TraversalCompletion::Complete; 2]);
            }
        }
    }

    #[test]
    fn unified_collection_keeps_grouping_limits_and_generic_metadata_paths() {
        use crate::{Attributes, ListDirOptions};
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.create_dir_all("/a/sub").unwrap();
        fs.create_dir("/b").unwrap();
        fs.write("/a/sub/f", b"payload").unwrap();
        fs.write("/b/f", b"x").unwrap();
        let shallow = ListDirOptions::new().fields(Attributes::SIZE);
        let listed = fs.read_dirs_with_options(&["/a", "/b"], shallow).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|tree| tree.len() == 1));
        assert_eq!(listed[0][0].path, Path::new("/a"));
        assert_eq!(listed[1][0].entries[0].attrs().len(), 1);
        let trees = fs
            .read_dirs_with_options(&["/a", "/b"], shallow.recursive(true))
            .unwrap();
        assert_eq!(trees[0].len(), 2);
        assert_eq!(trees[1].len(), 1);
        assert!(
            trees[0]
                .iter()
                .flat_map(|listing| &listing.entries)
                .any(|entry| entry.path() == Path::new("/a/sub/f") && entry.attrs().len() == 7)
        );
        assert_eq!(
            fs.read_dirs_with_options(&["/a", "/a"], shallow.max_entries(1))
                .unwrap_err()
                .index(),
            Some(1)
        );
        assert_eq!(
            fs.read_dirs_with_options(&["/a", "/missing"], shallow)
                .unwrap_err()
                .index(),
            Some(1)
        );
        assert!(
            fs.read_dirs_with_options::<&str>(&[], shallow)
                .unwrap()
                .is_empty()
        );
        let truncated = fs
            .read_dirs_with_options(
                &["/a"],
                shallow
                    .recursive(true)
                    .max_depth(0)
                    .truncate_at_max_depth(true),
            )
            .unwrap();
        assert_eq!(truncated[0].len(), 1);
        std::os::unix::fs::symlink(root.path().join("a"), root.path().join("link")).unwrap();
        let paths = vec![String::from("/link"), String::from("/b")];
        let metadata = fs
            .vgetattrs(&paths, crate::AttrsOptions::new().follow_symlinks(false))
            .unwrap();
        assert!(metadata[0].is_symlink());
        assert!(metadata[1].is_dir());
        assert!(
            fs.vgetattrs(
                &[std::path::PathBuf::from("/link")],
                crate::AttrsOptions::new().follow_symlinks(false)
            )
            .unwrap()[0]
                .is_symlink()
        );
        assert!(
            fs.vgetattrs(
                &["/link"],
                crate::AttrsOptions::new().follow_symlinks(false)
            )
            .unwrap()[0]
                .is_symlink()
        );
        assert!(
            fs.vgetattrs::<String>(&[], crate::AttrsOptions::new().follow_symlinks(false))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn unified_visiting_preserves_depth_defaults_fields_and_failures() {
        use crate::{Attributes, ControlFlow, ListDirOptions, TraversalCompletion};
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path())
                .unwrap()
                .with_limits(ResourceLimits {
                    max_directory_entries: 1,
                    ..Default::default()
                }),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        std::fs::create_dir_all(root.path().join("tree/sub/deep")).unwrap();
        std::fs::write(root.path().join("tree/sub/file"), b"payload").unwrap();
        std::fs::create_dir(root.path().join("other")).unwrap();
        std::os::unix::fs::symlink(root.path().join("tree/sub"), root.path().join("other/link"))
            .unwrap();
        let mut seen = Vec::new();
        fs.visit_entries_with_options(&["/tree"], ListDirOptions::new(), |_, entry| {
            seen.push(entry.path().to_path_buf());
            // Callback may reenter the same filesystem.
            assert!(fs.attrs(entry.path()).is_ok());
            Ok(ControlFlow::Continue(()))
        })
        .unwrap();
        assert_eq!(seen, [std::path::PathBuf::from("/tree/sub")]);
        assert!(
            fs.visit_entries_with_options(
                &["/tree"],
                ListDirOptions::new().recursive(true),
                |_, _| Ok(ControlFlow::Continue(()))
            )
            .is_err()
        ); // inherits client budget
        let recursive = ListDirOptions::new().recursive(true).max_entries(10);
        assert!(
            fs.visit_entries_with_options(&["/tree"], recursive.max_depth(0), |_, _| Ok(
                ControlFlow::Continue(())
            ))
            .is_err()
        );
        for (depth, count) in [(0, 1), (1, 3), (2, 3)] {
            let mut seen = Vec::new();
            fs.visit_entries_with_options(
                &["/tree"],
                recursive
                    .max_depth(depth)
                    .truncate_at_max_depth(true)
                    .fields(Attributes::SIZE),
                |_, entry| {
                    if entry.path().ends_with("file") {
                        assert_eq!(entry.attrs().len(), 7);
                    }
                    seen.push(entry.path().to_path_buf());
                    Ok(ControlFlow::Continue(()))
                },
            )
            .unwrap();
            assert_eq!(seen.len(), count);
        }
        // Symlink entries are delivered but never traversed.
        let mut seen = Vec::new();
        fs.visit_entries_with_options(&["/other"], recursive, |_, entry| {
            seen.push(entry.path().to_path_buf());
            Ok(ControlFlow::Continue(()))
        })
        .unwrap();
        assert_eq!(seen, [std::path::PathBuf::from("/other/link")]);
        assert_eq!(
            fs.visit_entries_with_options(&["/tree", "/missing"], recursive, |_, _| Ok(
                ControlFlow::Break(())
            ))
            .unwrap(),
            [TraversalCompletion::Stopped]
        );
        let error = fs
            .visit_entries_with_options(&["/other", "/tree"], recursive, |index, _| {
                if index == 1 {
                    Err(crate::Error::client(0, libc::EIO as u32))
                } else {
                    Ok(ControlFlow::Continue(()))
                }
            })
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert!(
            fs.visit_entries_with_options::<&str>(&[], recursive, |_, _| panic!("empty vector"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn removal_modes_preserve_roots_symlinks_and_vector_indices() {
        use crate::{RemoveMode, RemoveOptions};
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        // Probe has no inherent removal methods: these calls exercise Vfsi/VfsiExt.
        std::fs::create_dir_all(root.path().join("tree/nested")).unwrap();
        std::fs::write(root.path().join("tree/nested/file"), b"keep").unwrap();
        assert!(
            fs.vremove(&["/tree"], RemoveMode::Entry, Default::default())
                .is_err()
        );
        assert!(root.path().join("tree/nested/file").exists());
        fs.vremove(&["/tree"], RemoveMode::Contents, Default::default())
            .unwrap();
        assert!(root.path().join("tree").is_dir());
        assert!(
            std::fs::read_dir(root.path().join("tree"))
                .unwrap()
                .next()
                .is_none()
        );

        std::fs::create_dir(root.path().join("outside")).unwrap();
        std::fs::write(root.path().join("outside/file"), b"safe").unwrap();
        symlink(root.path().join("outside"), root.path().join("link")).unwrap();
        let error = fs
            .vremove(
                &["/tree", "/link"],
                RemoveMode::Contents,
                Default::default(),
            )
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert!(root.path().join("tree").is_dir());
        assert!(root.path().join("outside/file").exists());
        fs.vremove(&["/link"], RemoveMode::Tree, Default::default())
            .unwrap();
        assert!(root.path().join("outside/file").exists());
        fs.vremove(&["/outside"], RemoveMode::Tree, Default::default())
            .unwrap();
        assert!(!root.path().join("outside").exists());
        fs.vremove(&["/tree"], RemoveMode::Entry, Default::default())
            .unwrap();

        for mode in [RemoveMode::Entry, RemoveMode::Tree, RemoveMode::Contents] {
            fs.vremove::<&str>(&[], mode, RemoveOptions::new()).unwrap();
        }
        std::fs::create_dir(root.path().join("policy")).unwrap();
        std::fs::write(root.path().join("policy/file"), b"unchanged").unwrap();
        // The mounted generic remover rejects unsupported custom policy. Contents
        // must forward it rather than silently falling back to default options.
        assert!(
            fs.vremove(
                &["/policy"],
                RemoveMode::Contents,
                RemoveOptions::new().retries(0)
            )
            .is_err()
        );
        assert!(root.path().join("policy/file").exists());
    }

    // No inherent conveniences or explicit VfsiExt implementation.
    struct Probe {
        mounted: crate::Mounted,
        calls: Cell<usize>,
        shape: Cell<u8>,
    }
    impl Probe {
        fn inner(&self) -> &crate::Mounted {
            &self.mounted
        }
        fn read<'a>(
            &self,
            ops: impl IntoIterator<Item = crate::ReadOp<'a, crate::MountedFile>>,
            options: crate::ReadOptions,
        ) -> Result<Vec<crate::ReadResult>> {
            self.calls.set(self.calls.get() + 1);
            let ops: Vec<_> = ops.into_iter().collect();
            assert!(ops.iter().all(|op| op.whole_file_path().is_some()));
            if self.shape.get() == 6 {
                return Err(crate::Error::transport(None, "lost reply"));
            }
            let mut results = self.mounted.vread(ops, options)?;
            match self.shape.get() {
                1 => {
                    results.pop();
                }
                2 => {
                    results[0] = crate::ReadResult::buffered(0, results[0].read(), true);
                }
                4 => {
                    results[0] =
                        crate::ReadResult::owned(0, results[0].clone().into_data().unwrap(), false);
                }
                5 => {
                    results[0] =
                        crate::ReadResult::owned(1, results[0].clone().into_data().unwrap(), true);
                }
                7 => {
                    results.push(crate::ReadResult::owned(0, Vec::new(), true));
                }
                _ => {}
            }
            Ok(results)
        }
    }
    impl Vfsi for Probe {
        type File = crate::MountedFile;
        client_methods!(
            crate::Mounted,
            Probe::inner,
            Probe::read,
            std::convert::identity
        );
    }

    #[test]
    fn blanket_helpers_preserve_order_empty_files_limits_and_error_indices() {
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path())
                .unwrap()
                .with_limits(ResourceLimits {
                    max_read_bytes: 5,
                    ..Default::default()
                }),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.write_files(&[("/a", b"abc".as_slice()), ("/b", b"de"), ("/empty", b"")])
            .unwrap();
        assert_eq!(
            fs.read_files(&["/b", "/empty", "/a"]).unwrap(),
            [b"de".to_vec(), vec![], b"abc".to_vec()]
        );
        assert_eq!(
            fs.calls.get(),
            1,
            "one vector dispatch, not a scalar read loop"
        );
        assert!(fs.read_files::<&str>(&[]).unwrap().is_empty());
        assert_eq!(
            fs.read_files(&["/a", "/b", "/a"]).unwrap_err().kind(),
            crate::ErrorKind::FileTooLarge
        );
        assert_eq!(
            fs.read_files_with_options(
                &["/a", "/b", "/a"],
                crate::ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(8))
            )
            .unwrap()
            .len(),
            3
        );
        assert_eq!(
            fs.read_files(&["/a", "/missing"]).unwrap_err().index(),
            Some(1)
        );
        fs.write("/a", b"z").unwrap();
        assert_eq!(fs.read_to_string("/a").unwrap(), "z");
        fs.write("/a", &[0xff]).unwrap();
        assert_eq!(
            fs.read_to_string("/a").unwrap_err().kind(),
            crate::ErrorKind::InvalidInput
        );
        assert_eq!(
            fs.write_files(&[("/a", b"bad"), ("/a", b"bad")])
                .unwrap_err()
                .index(),
            Some(1)
        );
        assert_eq!(
            fs.read_files(&["/a"]).unwrap(),
            [vec![0xff]],
            "duplicate writes rejected before truncate"
        );
        fs.copy("/b", "/copy").unwrap();
        assert_eq!(fs.read_files(&["/copy"]).unwrap(), [b"de".to_vec()]);
        let mut file = fs.create("/created").unwrap();
        assert!(!file.is_closed());
        fs.vclose(std::slice::from_mut(&mut file)).unwrap();
        assert!(file.is_closed());
        fs.close_files(vec![file]).unwrap();
    }

    // Dispatch spy: model a backend accepting at most two bytes per request,
    // while delegating complete writes to the existing native implementation.
    // Native short-write waves and zero-progress checks have separate coverage
    // in vfsi-sync/tests/native_client.rs.
    struct WritePolicyProbe {
        mounted: crate::Mounted,
        partial_calls: Cell<usize>,
        complete_calls: Cell<usize>,
        lose_reply: Cell<bool>,
    }
    impl WritePolicyProbe {
        fn inner(&self) -> &crate::Mounted {
            &self.mounted
        }
        fn partial(
            &self,
            ops: &[crate::WriteOp<'_, crate::MountedFile>],
        ) -> Result<Vec<WriteResult>> {
            self.partial_calls.set(self.partial_calls.get() + 1);
            let short: Vec<_> = ops
                .iter()
                .map(|op| {
                    crate::WriteOp::at(op.file(), op.offset(), &op.data()[..op.data().len().min(2)])
                })
                .collect();
            self.mounted.vwrite(&short, Default::default())
        }
        fn complete(
            &self,
            ops: &[crate::WriteOp<'_, crate::MountedFile>],
        ) -> Result<Vec<WriteResult>> {
            self.complete_calls.set(self.complete_calls.get() + 1);
            if self.lose_reply.get() {
                self.partial(ops)?; // The server mutated data before the reply was lost.
                return Err(crate::Error::transport(None, "injected lost write reply"));
            }
            self.mounted
                .vwrite(ops, crate::WriteOptions::new().write_all(true))
        }
    }
    impl Vfsi for WritePolicyProbe {
        type File = crate::MountedFile;
        client_methods!(
            crate::Mounted,
            WritePolicyProbe::inner,
            crate::Mounted::vread,
            WritePolicyProbe::inner,
            WritePolicyProbe::partial,
            WritePolicyProbe::complete,
            crate::Mounted::vgetattrs,
            std::convert::identity
        );
    }

    #[test]
    fn write_options_select_completion_without_replaying_ambiguous_errors() {
        let root = tempfile::tempdir().unwrap();
        let fs = WritePolicyProbe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            partial_calls: Cell::new(0),
            complete_calls: Cell::new(0),
            lose_reply: Cell::new(false),
        };
        let file = fs.create("/file").unwrap();
        let ops = [crate::WriteOp::at(&file, 0, b"abcdef")];
        assert!(!crate::WriteOptions::default().writes_all());
        assert_eq!(fs.vwrite(&ops, Default::default()).unwrap()[0].written, 2);
        for options in [
            crate::WriteOptions::new(),
            crate::WriteOptions::new().write_all(true).write_all(false),
        ] {
            assert_eq!(fs.vwrite(&ops, options).unwrap()[0].written, 2);
        }
        assert_eq!(fs.partial_calls.get(), 3);
        assert_eq!(fs.complete_calls.get(), 0);
        assert_eq!(fs.read_files(&["/file"]).unwrap(), [b"ab".to_vec()]);
        let complete = crate::WriteOptions::new().write_all(true);
        assert_eq!(fs.vwrite(&ops, complete).unwrap()[0].written, 6);
        assert_eq!(fs.partial_calls.get(), 3);
        assert_eq!(fs.complete_calls.get(), 1);
        assert_eq!(fs.read_files(&["/file"]).unwrap(), [b"abcdef".to_vec()]);

        fs.lose_reply.set(true);
        let error = fs
            .vwrite(&[crate::WriteOp::at(&file, 0, b"UVWXYZ")], complete)
            .unwrap_err();
        assert!(error.is_transport());
        assert_eq!(error.index(), None);
        assert_eq!(fs.partial_calls.get(), 4);
        assert_eq!(
            fs.complete_calls.get(),
            2,
            "never replay an ambiguous failure"
        );
        assert_eq!(fs.read_files(&["/file"]).unwrap(), [b"UVcdef".to_vec()]);
        file.close().unwrap();
    }

    #[test]
    fn read_files_rejects_malformed_replies_and_never_replays_transport_errors() {
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.write("/a", b"abc").unwrap();
        // Owned constructors derive byte counts, so inconsistent counts are unrepresentable.
        for shape in [1, 2, 4, 5, 6, 7] {
            fs.shape.set(shape);
            let before = fs.calls.get();
            let error = fs.read_files(&["/a"]).unwrap_err();
            assert!(error.is_transport(), "shape {shape}: {error}");
            assert_eq!(fs.calls.get(), before + 1, "shape {shape} must not replay");
            if matches!(shape, 2..=5) {
                assert_eq!(error.index(), Some(0));
            }
        }
    }

    #[test]
    fn default_listing_and_stream_helpers_respect_client_limits_and_reentry() {
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path())
                .unwrap()
                .with_limits(ResourceLimits {
                    max_directory_entries: 1,
                    stream_chunk_bytes: 2,
                    ..Default::default()
                }),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.create_dir("/dir").unwrap();
        fs.write_files(&[("/dir/a", b"abc"), ("/dir/b", b"def")])
            .unwrap();
        assert!(fs.read_dir("/dir").is_err());
        assert!(fs.walk("/dir").is_err());
        let mut payload = Vec::new();
        fs.read_stream("/dir/a", |offset, bytes| {
            assert!(bytes.len() <= 2);
            assert_eq!(offset as usize, payload.len());
            // Reenter the same client from its callback.
            assert_eq!(fs.attrs("/dir/a")?.len(), 3);
            payload.extend_from_slice(bytes);
            Ok(true)
        })
        .unwrap();
        assert_eq!(payload, b"abc");
        fs.listdir("/dir", vfsi_core::api::ListDirOptions::new(), |_| {
            Ok(vfsi_core::api::WalkControl::Stop)
        })
        .unwrap();
    }

    #[test]
    fn vector_walk_budgets_count_entries_not_listing_containers() {
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.vmkdir(&[
            crate::MkDirOp::new("/a", 0o777),
            crate::MkDirOp::new("/b", 0o777),
        ])
        .unwrap();
        fs.write_files(&[("/a/f", b"x"), ("/b/f", b"y")]).unwrap();
        let options = crate::WalkOptions::new().max_entries(2);
        let trees = fs
            .read_dirs_with_options(
                &["/a", "/b"],
                crate::ListDirOptions::from(options).fields(crate::Attributes::MODE),
            )
            .unwrap();
        assert_eq!(
            trees
                .iter()
                .flat_map(|tree| tree.iter())
                .map(|listing| listing.entries.len())
                .sum::<usize>(),
            2
        );
        assert_eq!(
            fs.read_dirs_with_options(
                &["/a", "/b"],
                crate::ListDirOptions::from(options.max_entries(1)).fields(crate::Attributes::MODE)
            )
            .unwrap_err()
            .index(),
            Some(1)
        );
        fs.remove_file("/a/f").unwrap();
        fs.remove_file("/b/f").unwrap();
        assert!(
            fs.read_dirs_with_options(
                &["/a", "/b"],
                crate::ListDirOptions::from(options.max_entries(0)).fields(crate::Attributes::MODE)
            )
            .is_ok()
        );
        assert_eq!(
            fs.read_dirs_with_options(
                &["/a", "/b"],
                crate::ListDirOptions::from(options.max_path_bytes(3))
                    .fields(crate::Attributes::MODE)
            )
            .unwrap_err()
            .index(),
            Some(1)
        );
    }

    #[test]
    fn vector_walk_visitors_charge_root_paths_once_and_honor_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.vmkdir(&[
            crate::MkDirOp::new("/a", 0o777),
            crate::MkDirOp::new("/b", 0o777),
        ])
        .unwrap();
        let options = crate::WalkOptions::new().max_entries(0).max_path_bytes(4);
        assert_eq!(
            fs.visit_entries_with_options(&["/a", "/b"], options.into(), |_, _| panic!(
                "empty roots"
            ))
            .unwrap(),
            [crate::TraversalCompletion::Complete; 2]
        );
        assert_eq!(
            fs.visit_entries_with_options(
                &["/a", "/b"],
                options.max_path_bytes(2).into(),
                |_, _| panic!("empty roots")
            )
            .unwrap_err()
            .index(),
            Some(1)
        );
        fs.write_files(&[("/a/f", b"x"), ("/b/f", b"y")]).unwrap();
        let options = options.max_entries(2).max_path_bytes(12);
        let mut seen = Vec::new();
        fs.visit_entries_with_options(&["/a", "/b"], options.into(), |index, entry| {
            seen.push((index, entry.path().to_path_buf()));
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap();
        assert_eq!(
            seen,
            [
                (0, std::path::PathBuf::from("/a/f")),
                (1, std::path::PathBuf::from("/b/f"))
            ]
        );
        assert_eq!(
            fs.visit_entries_with_options(
                &["/a", "/b"],
                options.max_path_bytes(11).into(),
                |_, _| Ok(std::ops::ControlFlow::Continue(()))
            )
            .unwrap_err()
            .index(),
            Some(1)
        );
        assert_eq!(
            fs.visit_entries_with_options(
                &["/a", "/missing"],
                options.max_path_bytes(6).into(),
                |_, _| Ok(std::ops::ControlFlow::Break(()))
            )
            .unwrap(),
            [crate::TraversalCompletion::Stopped]
        );
    }

    #[test]
    fn singleton_walk_reports_the_root_index_not_an_entry_index() {
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.create_dir_all("/a/sub").unwrap();
        fs.write("/a/f", b"x").unwrap();
        let options = crate::WalkOptions::new().max_depth(0);
        // Concrete method syntax must use the same extension as generic code,
        // not leak the backend's per-entry index.
        assert_eq!(
            fs.mounted
                .walk_with_options(
                    "/a",
                    crate::ListDirOptions::from(options).fields(crate::Attributes::MODE)
                )
                .unwrap_err()
                .index(),
            Some(0)
        );
        assert_eq!(
            fs.read_dirs_with_options(
                &["/a"],
                crate::ListDirOptions::from(options).fields(crate::Attributes::MODE)
            )
            .unwrap_err()
            .index(),
            Some(0)
        );
    }

    #[test]
    fn blanket_scalar_helpers_and_new_vectors_preserve_limits_stop_and_indices() {
        let root = tempfile::tempdir().unwrap();
        // Probe implements only Vfsi; no explicit VfsiExt implementation is possible.
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fn scalar<C: Vfsi>(fs: &C) {
            fs.create_dir_all("/one/nested").unwrap();
            fs.create_dir_all("/two").unwrap();
            fs.write("/one/a", b"abc").unwrap();
            fs.write("/two/b", b"def").unwrap();
            fs.rename("/one/a", "/one/renamed").unwrap();
        }
        scalar(&fs);
        fs.vrename(
            &[("/one/renamed", "/one/a"), ("/two/b", "/two/c")],
            crate::RenameOptions::Replace,
        )
        .unwrap();
        let error = fs
            .vrename(
                &[("/one/a", "/one/moved"), ("/absent", "/two/moved")],
                crate::RenameOptions::Replace,
            )
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert!(root.path().join("one/moved").exists());

        let roots = ["/one", "/two"];
        let trees = fs
            .read_dirs_with_options(
                &roots,
                crate::ListDirOptions::from(crate::WalkOptions::new())
                    .fields(crate::Attributes::MODE),
            )
            .unwrap();
        assert_eq!(trees.len(), 2);
        assert!(trees.iter().all(|tree| !tree.is_empty()));
        for limit in 0..=4 {
            let options = crate::WalkOptions::new().max_entries(limit);
            assert_eq!(
                fs.walk_with_options(
                    "/two",
                    crate::ListDirOptions::from(options).fields(crate::Attributes::MODE)
                )
                .is_ok(),
                fs.mounted
                    .walk_with_options(
                        "/two",
                        crate::ListDirOptions::from(options).fields(crate::Attributes::MODE)
                    )
                    .is_ok()
            );
        }
        let mut seen = Vec::new();
        let completion = fs
            .visit_entries_with_options(&roots, crate::ListDirOptions::new(), |index, entry| {
                seen.push(index);
                assert!(fs.attrs(entry.path()).is_ok()); // callbacks can reenter
                Ok(std::ops::ControlFlow::Break(()))
            })
            .unwrap();
        assert_eq!(completion, [crate::TraversalCompletion::Stopped]);
        assert_eq!(seen, [0]);

        let mut seen = Vec::new();
        let completion = fs
            .visit_entries_with_options(
                &roots,
                crate::ListDirOptions::new().recursive(true),
                |index, _| {
                    seen.push(index);
                    Ok(std::ops::ControlFlow::Break(()))
                },
            )
            .unwrap();
        assert_eq!(completion, [crate::TraversalCompletion::Stopped]);
        assert_eq!(seen, [0]);

        let error = fs
            .visit_entries_with_options(
                &["/two", "/two"],
                crate::ListDirOptions::new().max_entries(1),
                |_, _| Ok(std::ops::ControlFlow::Continue(())),
            )
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        let error = fs
            .visit_entries_with_options(
                &["/two", "/two"],
                crate::ListDirOptions::new().max_path_bytes("/two/c".len()),
                |_, _| Ok(std::ops::ControlFlow::Continue(())),
            )
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert!(
            fs.read_dirs_with_options(
                &["/two", "/two"],
                crate::ListDirOptions::from(crate::WalkOptions::new().max_entries(1))
                    .fields(crate::Attributes::MODE)
            )
            .is_err()
        );

        let mut seen = Vec::new();
        let completion = fs
            .vstream(
                &["/one/moved", "/absent"],
                crate::StreamOptions::new().chunk_size(2),
                |index, offset, data| {
                    assert_eq!((index, offset), (0, 0));
                    assert_eq!(data, b"ab");
                    seen.push(index);
                    Ok(false)
                },
            )
            .unwrap();
        assert_eq!(
            completion,
            [crate::StreamCompletion::Stopped { next_offset: 2 }]
        );
        assert_eq!(seen, [0]);
        let error = fs
            .vstream(
                &["/one/moved", "/absent"],
                crate::StreamOptions::new(),
                |_, _, _| Ok(true),
            )
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert!(fs.remove_file("/one").is_err());
        assert!(fs.remove_dir("/one/moved").is_err());
        fs.vremove(
            &roots,
            crate::RemoveMode::Contents,
            crate::RemoveOptions::new(),
        )
        .unwrap();
        assert!(fs.read_dir("/one").unwrap().is_empty());
        assert!(fs.read_dir("/two").unwrap().is_empty());
        fs.remove_dir("/two").unwrap();
        fs.remove_dir_all("/one").unwrap();
        assert!(fs.read_dir("/").unwrap().is_empty());
    }
}
