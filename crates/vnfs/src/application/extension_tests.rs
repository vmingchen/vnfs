use super::*;
use crate::VfsiExt;
use std::cell::Cell;
use vfsi_sync::application::visit_directory_pages;

#[test]
fn page_faults_do_not_replay_transport_failures_and_reject_malformed_shapes() {
    use crate::{ControlFlow, ListDirOptions, ResourceLimits};
    for malformed in 0..4 {
        let mut calls = 0;
        let result = {
            let page_inputs = &["/a"];
            vfsi_core::internal::collect_vec_result(
                visit_directory_pages(
                    page_inputs,
                    ListDirOptions::new(),
                    ResourceLimits::default(),
                    |_| Ok(32),
                    |_| Ok(()),
                    |_, _, _, _, _| {
                        calls += 1;
                        match malformed {
                            0 => Err(crate::Error::transport(None, "lost reply")),
                            1 => Ok(Vec::new()),
                            2 => Ok(vec![Ok((
                                DirectoryListing {
                                    path: "/wrong".into(),
                                    entries: Vec::new(),
                                },
                                None,
                                Vec::new(),
                            ))]),
                            _ => Ok(vec![Ok((
                                DirectoryListing {
                                    path: "/a".into(),
                                    entries: Vec::new(),
                                },
                                Some(vfsi_sync::DirPageCursor::new(0usize)),
                                Vec::new(),
                            ))]),
                        }
                    },
                    |_, _| -> Result<ControlFlow<()>> {
                        panic!("invalid pages must not be delivered")
                    },
                ),
                page_inputs.len(),
            )
        };
        assert!(result.unwrap_err().is_transport());
        assert_eq!(calls, 1);
    }
    let mut calls = 0;
    let result = visit_directory_pages(
        &["/a", "/missing"],
        ListDirOptions::new(),
        ResourceLimits::default(),
        |_| Ok(32),
        |_| Ok(()),
        |_, _, _, _, _| {
            calls += 1;
            Ok(vec![
                Ok((
                    DirectoryListing {
                        path: "/a".into(),
                        entries: vec![],
                    },
                    None,
                    vec![],
                )),
                Err(vfsi_core::OpError::failed(
                    crate::Error::errno(None, libc::ENOENT as u32),
                    None,
                )),
            ])
        },
        |index, page| {
            assert_eq!(index, 0);
            assert!(page.entries.is_empty());
            Ok(ControlFlow::Break(()))
        },
    )
    .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0], Ok(crate::TraversalCompletion::Stopped));
    assert_eq!(
        result[1].as_ref().unwrap_err().cause().raw_code(),
        libc::ENOENT as u32
    );
    assert_eq!(
        calls, 1,
        "successful pages and failed slots are consumed without prefix replay"
    );
}

#[test]
fn directory_continuations_run_in_vector_waves_and_cancellation_marks_unfinished_roots() {
    use crate::{ControlFlow, ListDirOptions, ResourceLimits};
    let temp = tempfile::tempdir().unwrap();
    let mounted = crate::Posix::new(temp.path()).unwrap();
    mounted.write("/f", b"x").unwrap();
    let metadata = mounted.attrs("/f").unwrap();
    for stop in [false, true] {
        let mut widths = Vec::new();
        let result = visit_directory_pages(
            &["/a", "/b"],
            ListDirOptions::new(),
            ResourceLimits::default(),
            |_| Ok(32),
            |_| Ok(()),
            |paths, cursors, _, _, _| {
                widths.push(paths.len());
                Ok(paths
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
                    .map(|result: Result<_>| {
                        result.map_err(|cause| vfsi_core::OpError::failed(cause, None))
                    })
                    .collect())
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
            assert_eq!(
                result,
                [
                    Ok(crate::TraversalCompletion::Stopped),
                    Ok(crate::TraversalCompletion::Stopped)
                ]
            );
        } else {
            assert_eq!(widths, [2, 2, 2]);
            assert_eq!(
                result,
                [
                    Ok(crate::TraversalCompletion::Complete),
                    Ok(crate::TraversalCompletion::Complete)
                ]
            );
        }
    }
}

#[test]
fn unified_collection_keeps_grouping_limits_and_generic_metadata_paths() {
    use crate::{Attributes, ListDirOptions};
    let root = tempfile::tempdir().unwrap();
    let fs = Probe {
        mounted: crate::Posix::new(root.path()).unwrap(),
        calls: Cell::new(0),
        shape: Cell::new(0),
    };
    fs.create_dir_all("/a/sub").unwrap();
    fs.create_dir("/b").unwrap();
    fs.write("/a/sub/f", b"payload").unwrap();
    fs.write("/b/f", b"x").unwrap();
    let shallow = ListDirOptions::new().with_fields(Attributes::SIZE);
    let listed = fs.read_dirs_with_options(&["/a", "/b"], shallow).unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|tree| tree.len() == 1));
    assert_eq!(listed[0][0].path, Path::new("/a"));
    assert_eq!(listed[1][0].entries[0].attrs().len().unwrap(), 1);
    let trees = fs
        .read_dirs_with_options(&["/a", "/b"], shallow.with_recursive(true))
        .unwrap();
    assert_eq!(trees[0].len(), 2);
    assert_eq!(trees[1].len(), 1);
    assert!(
        trees[0]
            .iter()
            .flat_map(|listing| &listing.entries)
            .any(|entry| entry.path() == Path::new("/a/sub/f")
                && entry.attrs().len().unwrap() == 7)
    );
    assert_eq!(
        fs.read_dirs_with_options(&["/a", "/a"], shallow.with_max_entries(1))
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
                .with_recursive(true)
                .with_max_depth(vfsi_core::api::DepthLimit::new(0).expect("finite depth"))
                .with_truncate_at_max_depth(true),
        )
        .unwrap();
    assert_eq!(truncated[0].len(), 1);
    std::os::unix::fs::symlink(root.path().join("a"), root.path().join("link")).unwrap();
    let paths = vec![String::from("/link"), String::from("/b")];
    let metadata = fs
        .getattrs_all(
            &paths,
            crate::AttrsOptions::new().with_follow_final_symlink(false),
        )
        .unwrap();
    assert!(metadata[0].is_symlink());
    assert!(metadata[1].is_dir());
    assert!(
        fs.getattrs_all(
            &[std::path::PathBuf::from("/link")],
            crate::AttrsOptions::new().with_follow_final_symlink(false)
        )
        .unwrap()[0]
            .is_symlink()
    );
    assert!(
        fs.getattrs_all(
            &["/link"],
            crate::AttrsOptions::new().with_follow_final_symlink(false)
        )
        .unwrap()[0]
            .is_symlink()
    );
    assert!(
        fs.getattrs_all::<String>(
            &[],
            crate::AttrsOptions::new().with_follow_final_symlink(false)
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn unified_visiting_preserves_depth_defaults_fields_and_failures() {
    use crate::{Attributes, ControlFlow, ListDirOptions, TraversalCompletion};
    let root = tempfile::tempdir().unwrap();
    let fs = Probe {
        mounted: crate::Posix::new(root.path())
            .unwrap()
            .with_limits(ResourceLimits::new().with_max_directory_entries(1)),
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
            ListDirOptions::new().with_recursive(true),
            |_, _| Ok(ControlFlow::Continue(()))
        )
        .is_err()
    ); // inherits client budget
    let recursive = ListDirOptions::new()
        .with_recursive(true)
        .with_max_entries(10);
    assert!(
        fs.visit_entries_with_options(
            &["/tree"],
            recursive.with_max_depth(vfsi_core::api::DepthLimit::new(0).expect("finite depth")),
            |_, _| Ok(ControlFlow::Continue(()))
        )
        .is_err()
    );
    for (depth, count) in [(0, 1), (1, 3), (2, 3)] {
        let mut seen = Vec::new();
        fs.visit_entries_with_options(
            &["/tree"],
            recursive
                .with_max_depth(if depth == usize::MAX {
                    vfsi_core::api::DepthLimit::unlimited()
                } else {
                    vfsi_core::api::DepthLimit::new(depth).expect("finite depth")
                })
                .with_truncate_at_max_depth(true)
                .with_fields(Attributes::SIZE),
            |_, entry| {
                if entry.path().ends_with("file") {
                    assert_eq!(entry.attrs().len().unwrap(), 7);
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
        [TraversalCompletion::Stopped; 2]
    );
    let error = fs
        .visit_entries_with_options(&["/other", "/tree"], recursive, |index, _| {
            if index == 1 {
                Err(crate::Error::errno(0, libc::EIO as u32))
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
        mounted: crate::Posix::new(root.path()).unwrap(),
        calls: Cell::new(0),
        shape: Cell::new(0),
    };
    // Probe has no inherent removal methods: these calls exercise Vfsi/VfsiExt.
    std::fs::create_dir_all(root.path().join("tree/nested")).unwrap();
    std::fs::write(root.path().join("tree/nested/file"), b"keep").unwrap();
    assert!(
        fs.removes_all(&["/tree"], RemoveMode::Entry, Default::default())
            .is_err()
    );
    assert!(root.path().join("tree/nested/file").exists());
    fs.removes_all(&["/tree"], RemoveMode::Contents, Default::default())
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
        .removes_all(
            &["/tree", "/link"],
            RemoveMode::Contents,
            Default::default(),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert!(root.path().join("tree").is_dir());
    assert!(root.path().join("outside/file").exists());
    fs.removes_all(&["/link"], RemoveMode::Tree, Default::default())
        .unwrap();
    assert!(root.path().join("outside/file").exists());
    fs.removes_all(&["/outside"], RemoveMode::Tree, Default::default())
        .unwrap();
    assert!(!root.path().join("outside").exists());
    fs.removes_all(&["/tree"], RemoveMode::Entry, Default::default())
        .unwrap();

    for mode in [RemoveMode::Entry, RemoveMode::Tree, RemoveMode::Contents] {
        fs.vremove::<&str>(&[], mode, RemoveOptions::new()).unwrap();
    }
    std::fs::create_dir(root.path().join("policy")).unwrap();
    std::fs::write(root.path().join("policy/file"), b"unchanged").unwrap();
    // The mounted generic remover rejects unsupported custom policy. Contents
    // must forward it rather than silently falling back to default options.
    assert!(
        fs.removes_all(
            &["/policy"],
            RemoveMode::Contents,
            RemoveOptions::new().with_retries(0)
        )
        .is_err()
    );
    assert!(root.path().join("policy/file").exists());
}

// No inherent conveniences or explicit VfsiExt implementation.
struct Probe {
    mounted: crate::Posix,
    calls: Cell<usize>,
    shape: Cell<u8>,
}
impl Probe {
    fn inner(&self) -> &crate::Posix {
        &self.mounted
    }
    fn read<'a>(
        &self,
        ops: impl IntoIterator<Item = crate::ReadOp<'a, crate::PosixFile>>,
        options: crate::ReadOptions,
    ) -> vfsi_core::VecResult<crate::ReadResult> {
        self.calls.set(self.calls.get() + 1);
        let ops: Vec<_> = ops.into_iter().collect();
        if !ops.iter().all(|op| op.whole_file_path().is_some()) {
            // Cohort streams use descriptor ranges. Keep whole-file fault
            // injection scoped to read_files rather than corrupting streams.
            assert_eq!(self.shape.get(), 0);
            return self.mounted.vread(ops, options);
        }
        if self.shape.get() == 6 {
            return Err(crate::Error::transport(None, "lost reply"));
        }
        let mut results = self.mounted.vread(ops, options)?;
        match self.shape.get() {
            1 => {
                results.pop();
            }
            2 => {
                let read = results[0].as_mut().unwrap();
                *read = crate::ReadResult::buffered(0, read.bytes_read(), true);
            }
            4 => {
                let read = results[0].as_mut().unwrap();
                *read = crate::ReadResult::owned(0, read.clone().into_data().unwrap(), false);
            }
            5 => {
                let read = results[0].as_mut().unwrap();
                *read = crate::ReadResult::owned(1, read.clone().into_data().unwrap(), true);
            }
            7 => {
                results.push(Ok(crate::ReadResult::owned(0, Vec::new(), true)));
            }
            _ => {}
        }
        Ok(results)
    }
}
impl Vfsi for Probe {
    type File = crate::PosixFile;
    type Dir = crate::PosixDir;
    client_methods!(
        crate::Posix,
        Probe::inner,
        Probe::read,
        std::convert::identity
    );
}

#[test]
fn blanket_helpers_preserve_order_empty_files_limits_and_error_indices() {
    let root = tempfile::tempdir().unwrap();
    let fs = Probe {
        mounted: crate::Posix::new(root.path())
            .unwrap()
            .with_limits(ResourceLimits::new().with_max_read_bytes(5)),
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
            crate::ReadOptions::new().with_max_total_bytes(std::num::NonZeroUsize::new(8).unwrap())
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
    fs.close_batch(std::slice::from_mut(&mut file)).unwrap();
    assert!(file.is_closed());
    fs.close_files(vec![file]).unwrap();
}

// Dispatch spy: model a backend accepting at most two bytes per request,
// while delegating complete writes to the existing native implementation.
// Native short-write waves and zero-progress checks have separate coverage
// in vfsi-sync/tests/native_client.rs.
struct WritePolicyProbe {
    mounted: crate::Posix,
    partial_calls: Cell<usize>,
    complete_calls: Cell<usize>,
    lose_reply: Cell<bool>,
}
impl WritePolicyProbe {
    fn inner(&self) -> &crate::Posix {
        &self.mounted
    }
    fn partial(
        &self,
        ops: &[crate::WriteOp<'_, crate::PosixFile>],
    ) -> vfsi_core::VecResult<WriteResult> {
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
        ops: &[crate::WriteOp<'_, crate::PosixFile>],
    ) -> vfsi_core::VecResult<WriteResult> {
        self.complete_calls.set(self.complete_calls.get() + 1);
        if self.lose_reply.get() {
            self.partial(ops)?; // The server mutated data before the reply was lost.
            return Ok(ops
                .iter()
                .map(|_| {
                    Err(vfsi_core::OpError::unknown(
                        crate::Error::transport(None, "injected lost write reply"),
                        None,
                    ))
                })
                .collect());
        }
        self.mounted
            .vwrite(ops, crate::WriteOptions::new().with_write_all(true))
    }
}
impl Vfsi for WritePolicyProbe {
    type File = crate::PosixFile;
    type Dir = crate::PosixDir;
    client_methods!(
        crate::Posix,
        WritePolicyProbe::inner,
        crate::Posix::vread_impl,
        WritePolicyProbe::inner,
        WritePolicyProbe::partial,
        WritePolicyProbe::complete,
        crate::Posix::vgetattrs_impl,
        std::convert::identity
    );
}

#[test]
fn write_options_select_completion_without_replaying_ambiguous_errors() {
    let root = tempfile::tempdir().unwrap();
    let fs = WritePolicyProbe {
        mounted: crate::Posix::new(root.path()).unwrap(),
        partial_calls: Cell::new(0),
        complete_calls: Cell::new(0),
        lose_reply: Cell::new(false),
    };
    let file = fs.create("/file").unwrap();
    let ops = [crate::WriteOp::at(&file, 0, b"abcdef")];
    assert!(!crate::WriteOptions::default().write_all());
    assert_eq!(
        fs.write_batch(&ops, Default::default()).unwrap()[0].bytes_written,
        2
    );
    for options in [
        crate::WriteOptions::new(),
        crate::WriteOptions::new()
            .with_write_all(true)
            .with_write_all(false),
    ] {
        assert_eq!(fs.write_batch(&ops, options).unwrap()[0].bytes_written, 2);
    }
    assert_eq!(fs.partial_calls.get(), 3);
    assert_eq!(fs.complete_calls.get(), 0);
    assert_eq!(fs.read_files(&["/file"]).unwrap(), [b"ab".to_vec()]);
    let complete = crate::WriteOptions::new().with_write_all(true);
    assert_eq!(fs.write_batch(&ops, complete).unwrap()[0].bytes_written, 6);
    assert_eq!(fs.partial_calls.get(), 3);
    assert_eq!(fs.complete_calls.get(), 1);
    assert_eq!(fs.read_files(&["/file"]).unwrap(), [b"abcdef".to_vec()]);

    fs.lose_reply.set(true);
    let error = fs
        .write_batch(&[crate::WriteOp::at(&file, 0, b"UVWXYZ")], complete)
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), Some(0));
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
        mounted: crate::Posix::new(root.path()).unwrap(),
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
        mounted: crate::Posix::new(root.path()).unwrap().with_limits(
            ResourceLimits::new()
                .with_max_directory_entries(1)
                .with_stream_chunk_size(std::num::NonZeroUsize::new(2).unwrap()),
        ),
        calls: Cell::new(0),
        shape: Cell::new(0),
    };
    fs.create_dir("/dir").unwrap();
    fs.write_files(&[("/dir/a", b"abc"), ("/dir/b", b"def")])
        .unwrap();
    assert!(fs.read_dir("/dir").is_err());
    assert!(
        fs.read_dirs_with_options(
            &["/dir"],
            crate::ListDirOptions::new()
                .with_fields(crate::Attributes::stat())
                .with_recursive(true)
        )
        .map(|mut trees| trees.remove(0))
        .is_err()
    );
    let mut payload = Vec::new();
    fs.read_stream("/dir/a", |offset, bytes| {
        assert!(bytes.len() <= 2);
        assert_eq!(offset as usize, payload.len());
        // Reenter the same client from its callback.
        assert_eq!(fs.attrs("/dir/a")?.len(), Some(3));
        payload.extend_from_slice(bytes);
        Ok(std::ops::ControlFlow::Continue(()))
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
        mounted: crate::Posix::new(root.path()).unwrap(),
        calls: Cell::new(0),
        shape: Cell::new(0),
    };
    fs.vmkdir(&[
        crate::MkDirOp::new("/a", 0o777),
        crate::MkDirOp::new("/b", 0o777),
    ])
    .and_then(|outcomes| {
        let count = outcomes.len();
        vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
    })
    .unwrap();
    fs.write_files(&[("/a/f", b"x"), ("/b/f", b"y")]).unwrap();
    let options = crate::ListDirOptions::new()
        .with_recursive(true)
        .with_max_entries(2);
    let trees = fs
        .read_dirs_with_options(&["/a", "/b"], options.with_fields(crate::Attributes::MODE))
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
            options
                .with_max_entries(1)
                .with_fields(crate::Attributes::MODE)
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
            options
                .with_max_entries(0)
                .with_fields(crate::Attributes::MODE)
        )
        .is_ok()
    );
    assert_eq!(
        fs.read_dirs_with_options(
            &["/a", "/b"],
            options
                .with_max_path_bytes(3)
                .with_fields(crate::Attributes::MODE)
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
        mounted: crate::Posix::new(root.path()).unwrap(),
        calls: Cell::new(0),
        shape: Cell::new(0),
    };
    fs.vmkdir(&[
        crate::MkDirOp::new("/a", 0o777),
        crate::MkDirOp::new("/b", 0o777),
    ])
    .and_then(|outcomes| {
        let count = outcomes.len();
        vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
    })
    .unwrap();
    let options = crate::ListDirOptions::new()
        .with_recursive(true)
        .with_max_entries(0)
        .with_max_path_bytes(4);
    assert_eq!(
        fs.visit_entries_with_options(&["/a", "/b"], options, |_, _| panic!("empty roots"))
            .unwrap(),
        [crate::TraversalCompletion::Complete; 2]
    );
    assert_eq!(
        fs.visit_entries_with_options(
            &["/a", "/b"],
            options.with_max_path_bytes(2),
            |_, _| panic!("empty roots")
        )
        .unwrap_err()
        .index(),
        Some(1)
    );
    fs.write_files(&[("/a/f", b"x"), ("/b/f", b"y")]).unwrap();
    let options = options.with_max_entries(2).with_max_path_bytes(12);
    let mut seen = Vec::new();
    fs.visit_entries_with_options(&["/a", "/b"], options, |index, entry| {
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
        fs.visit_entries_with_options(&["/a", "/b"], options.with_max_path_bytes(11), |_, _| Ok(
            std::ops::ControlFlow::Continue(())
        ))
        .unwrap_err()
        .index(),
        Some(1)
    );
    assert_eq!(
        fs.visit_entries_with_options(
            &["/a", "/missing"],
            options.with_max_path_bytes(6),
            |_, _| Ok(std::ops::ControlFlow::Break(()))
        )
        .unwrap(),
        [crate::TraversalCompletion::Stopped; 2]
    );
}

#[test]
fn singleton_walk_reports_the_root_index_not_an_entry_index() {
    let root = tempfile::tempdir().unwrap();
    let fs = Probe {
        mounted: crate::Posix::new(root.path()).unwrap(),
        calls: Cell::new(0),
        shape: Cell::new(0),
    };
    fs.create_dir_all("/a/sub").unwrap();
    fs.write("/a/f", b"x").unwrap();
    let options = crate::ListDirOptions::new()
        .with_recursive(true)
        .with_max_depth(vfsi_core::api::DepthLimit::new(0).expect("finite depth"));
    // Concrete method syntax must use the same extension as generic code,
    // not leak the backend's per-entry index.
    assert_eq!(
        fs.mounted
            .read_dirs_with_options(
                &["/a"],
                options
                    .with_fields(crate::Attributes::MODE)
                    .with_recursive(true)
            )
            .map(|mut trees| trees.remove(0))
            .unwrap_err()
            .index(),
        Some(0)
    );
    assert_eq!(
        fs.read_dirs_with_options(&["/a"], options.with_fields(crate::Attributes::MODE))
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
        mounted: crate::Posix::new(root.path()).unwrap(),
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
    fs.renames_all(
        &[("/one/renamed", "/one/a"), ("/two/b", "/two/c")],
        crate::RenameMode::Replace,
    )
    .unwrap();
    let error = fs
        .renames_all(
            &[("/one/a", "/one/moved"), ("/absent", "/two/moved")],
            crate::RenameMode::Replace,
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert!(root.path().join("one/moved").exists());

    let roots = ["/one", "/two"];
    let trees = fs
        .read_dirs_with_options(
            &roots,
            crate::ListDirOptions::new()
                .with_recursive(true)
                .with_fields(crate::Attributes::MODE),
        )
        .unwrap();
    assert_eq!(trees.len(), 2);
    assert!(trees.iter().all(|tree| !tree.is_empty()));
    for limit in 0..=4 {
        let options = crate::ListDirOptions::new()
            .with_recursive(true)
            .with_max_entries(limit);
        assert_eq!(
            fs.read_dirs_with_options(
                &["/two"],
                options
                    .with_fields(crate::Attributes::MODE)
                    .with_recursive(true)
            )
            .map(|mut trees| trees.remove(0))
            .is_ok(),
            fs.mounted
                .read_dirs_with_options(
                    &["/two"],
                    options
                        .with_fields(crate::Attributes::MODE)
                        .with_recursive(true)
                )
                .map(|mut trees| trees.remove(0))
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
    assert_eq!(completion, [crate::TraversalCompletion::Stopped; 2]);
    assert_eq!(seen, [0]);

    let mut seen = Vec::new();
    let completion = fs
        .visit_entries_with_options(
            &roots,
            crate::ListDirOptions::new().with_recursive(true),
            |index, _| {
                seen.push(index);
                Ok(std::ops::ControlFlow::Break(()))
            },
        )
        .unwrap();
    assert_eq!(completion, [crate::TraversalCompletion::Stopped; 2]);
    assert_eq!(seen, [0]);

    let error = fs
        .visit_entries_with_options(
            &["/two", "/two"],
            crate::ListDirOptions::new().with_max_entries(1),
            |_, _| Ok(std::ops::ControlFlow::Continue(())),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    let error = fs
        .visit_entries_with_options(
            &["/two", "/two"],
            crate::ListDirOptions::new().with_max_path_bytes("/two/c".len()),
            |_, _| Ok(std::ops::ControlFlow::Continue(())),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert!(
        fs.read_dirs_with_options(
            &["/two", "/two"],
            crate::ListDirOptions::new()
                .with_recursive(true)
                .with_max_entries(1)
                .with_fields(crate::Attributes::MODE)
        )
        .is_err()
    );

    let mut seen = Vec::new();
    let completion = fs
        .vstream(
            &["/one/moved", "/absent"],
            crate::StreamOptions::new().with_chunk_size(std::num::NonZeroUsize::new(2).unwrap()),
            |index, offset, data| {
                assert_eq!((index, offset), (0, 0));
                assert_eq!(data, b"ab");
                seen.push(index);
                Ok(std::ops::ControlFlow::Break(()))
            },
        )
        .unwrap();
    assert_eq!(completion.len(), 2);
    assert_eq!(
        completion[0].as_ref().unwrap(),
        &crate::StreamCompletion::Stopped { next_offset: 2 }
    );
    let error = completion[1].as_ref().unwrap_err();
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
    assert_eq!(seen, [0]);
    let error = fs
        .streams_all(
            &["/one/moved", "/absent"],
            crate::StreamOptions::new(),
            |_, _, _| Ok(std::ops::ControlFlow::Continue(())),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert!(fs.remove_file("/one").is_err());
    assert!(fs.remove_dir("/one/moved").is_err());
    fs.removes_all(
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
