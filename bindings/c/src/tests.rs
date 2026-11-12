use super::*;
use std::ffi::CString;

#[cfg(target_os = "linux")]
#[test]
fn c_scalar_and_vector_opens_honor_directory_and_nofollow_flags() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), b"preserved").unwrap();
    std::os::unix::fs::symlink("file", root.path().join("link")).unwrap();
    let fs = make_fs(
        Box::new(vfsi_posix::backend(root.path()).unwrap()),
        PathBuf::from("/"),
        PathBuf::from("/"),
    );
    for vector in [false, true] {
        for (path, flags, expected) in [
            ("/file", libc::O_RDONLY | libc::O_DIRECTORY, libc::ENOTDIR),
            (
                "/link",
                libc::O_WRONLY | libc::O_TRUNC | libc::O_NOFOLLOW,
                libc::ELOOP,
            ),
        ] {
            let path = CString::new(path).unwrap();
            if vector {
                let mut op = vfsi_open_op {
                    path: path.as_ptr(),
                    flags,
                    mode: 0o600,
                    fd: -1,
                };
                let mut item = vfsi_result::success(0);
                let result = unsafe { vfsi_openv(fs, &mut op, 1, &mut item) };
                assert_eq!(result.category, VFSI_ERROR_FILESYSTEM);
                assert_eq!(result.err_no, expected as u32);
                assert_eq!(item.err_no, expected as u32);
                assert_eq!(op.fd, -1);
            } else {
                assert_eq!(
                    unsafe { vfsi_open(fs, path.as_ptr(), flags, 0o600) },
                    -expected
                );
            }
            assert_eq!(
                std::fs::read(root.path().join("file")).unwrap(),
                b"preserved"
            );
        }
    }
    unsafe { vfsi_free(fs) };
}

#[test]
fn c_boolean_fields_accept_only_zero_and_one() {
    assert_eq!(parse_c_bool(0), Some(false));
    assert_eq!(parse_c_bool(1), Some(true));
    for value in [2, u8::MAX] {
        assert_eq!(parse_c_bool(value), None);
    }
    assert_eq!(std::mem::size_of::<bool>(), std::mem::size_of::<u8>());
    assert_eq!(std::mem::align_of::<bool>(), std::mem::align_of::<u8>());
}

#[test]
fn c_apis_reject_invalid_boolean_bytes_before_backend_io() {
    unsafe extern "C" fn stop_walk(
        _: *const c_char,
        _: u32,
        _: usize,
        _: *const vfsi_attrs,
        _: *mut c_void,
    ) -> u32 {
        0
    }
    let fixture = ListingFixture::new(0, AttrMask::empty(), 0);
    let root = CString::new("/").unwrap();
    let walk_options = vfsi_walk_options {
        max_entries: 1,
        max_path_bytes: 1024,
        max_depth: 1,
        attributes: VFSI_ATTR_MODE,
        sort_by_name: 2,
    };
    assert_eq!(
        unsafe {
            vfsi_walk(
                fixture.fs,
                root.as_ptr(),
                &walk_options,
                Some(stop_walk),
                std::ptr::null_mut(),
            )
        },
        libc::EINVAL
    );
    assert!(fixture.calls.lock().unwrap().is_empty());

    let src = CString::new("/src").unwrap();
    let dst = CString::new("/dst").unwrap();
    let copy = [vfsi_copy_op {
        src: src.as_ptr(),
        src_offset: 0,
        dst: dst.as_ptr(),
        dst_offset: 0,
        length: 1,
        to_eof: 2,
    }];
    let mut item = [vfsi_result::panic()];
    let result = unsafe { vfsi_copyv(fixture.fs, copy.as_ptr(), 1, item.as_mut_ptr()) };
    assert_eq!(result.category, VFSI_ERROR_INVALID_ARGUMENT);
    assert_eq!(result.err_no, libc::EINVAL as u32);
    assert_eq!(result.index, 0);
    assert_eq!(item[0].category, VFSI_ERROR_INVALID_ARGUMENT);
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[derive(Default)]
struct ListingCall {
    directories: Vec<PathBuf>,
    max_entries: usize,
}

type WriteCalls = std::sync::Arc<Mutex<Vec<Vec<(usize, usize)>>>>;
type CloseAttempts = std::sync::Arc<Mutex<Vec<i32>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum MalformedReply {
    OpenDuplicate,
    OpenPath,
    OpenNegativeFd,
    ListingNulName,
    ListingMissingName,
    ListingNulDirectory,
    ReadMiddleTransport,
    ReadReject,
    WriteReject,
    ReadPanic,
    WriteMiddlePartial,
    ReadWrongFile,
    ReadWrongOffset,
    ReadNoProgress,
    WriteWrongFile,
    WriteWrongOffset,
    WriteTooManyBytes,
}

// Exercise the actual C entry point. Like NFS, this backend fetches all
// roots' first pages before its first quota callback, and records that work.
struct ListingBackend {
    calls: std::sync::Arc<Mutex<Vec<ListingCall>>>,
    entries_per_dir: usize,
    emitted: usize,
    missing: AttrMask,
    missing_after: usize,
    fail_call: Option<usize>,
    write_calls: Option<WriteCalls>,
    open_preflight_error: Option<usize>,
    read_size: Option<usize>,
    open_result_count: Option<usize>,
    read_result_count: Option<usize>,
    write_result_count: Option<usize>,
    close_attempts: Option<CloseAttempts>,
    close_failure_fd: Option<i32>,
    malformed_reply: Option<MalformedReply>,
}

impl vfsi_sync::backend::HandleBackend for ListingBackend {
    fn vfsync_impl(
        &mut self,
        files: &[FileOperand],
        _mode: vfsi_sync::api::SyncMode,
    ) -> vfsi_sync::VecResult<()> {
        Ok(files
            .iter()
            .map(|file| {
                self.sync_data(file)
                    .map_err(|cause| vfsi_sync::OpError::unknown(cause, None))
            })
            .collect())
    }
    fn sync_data(&mut self, _: &FileOperand) -> vfsi_sync::VfResult<()> {
        Err(VfError::unsupported(0))
    }
    fn open_impl(&mut self, _: &vfsi_sync::OpenOp) -> vfsi_sync::VfResult<FileOperand> {
        Err(VfError::unsupported(0))
    }
    fn vclose_impl(&mut self, files: &[vfsi_sync::FileOperand]) -> vfsi_sync::VecResult<()> {
        Ok(files
            .iter()
            .map(|file| {
                self.close_impl(file)
                    .map_err(|error| vfsi_sync::OpError::unknown(error, None))
            })
            .collect())
    }

    fn close_impl(&mut self, file: &FileOperand) -> vfsi_sync::VfResult<()> {
        if let Some(attempts) = &self.close_attempts {
            attempts.lock().unwrap().push(file.fd().unwrap_or(-1));
        }
        if self
            .close_failure_fd
            .is_some_and(|fd| file.fd() == Some(fd))
        {
            Err(VfError::errno(0, libc::EIO as u32))
        } else {
            Ok(())
        }
    }
    fn read_impl(&mut self, _: &ReadOp) -> vfsi_sync::VfResult<vfsi_sync::ReadResult> {
        Err(VfError::unsupported(0))
    }
    fn write_impl(
        &mut self,
        _: vfsi_sync::WriteOp<&FileOperand, &[u8]>,
    ) -> vfsi_sync::VfResult<vfsi_sync::WriteResult> {
        Err(VfError::unsupported(0))
    }
    fn seek_impl(&mut self, _: &FileOperand, _: std::io::SeekFrom) -> vfsi_sync::VfResult<u64> {
        Err(VfError::unsupported(0))
    }
    fn metadata_impl(
        &mut self,
        _: vfsi_sync::Target<'_, FileOperand>,
        _: vfsi_sync::api::AttrsOptions,
    ) -> vfsi_sync::VfResult<VfAttrs> {
        Err(VfError::unsupported(0))
    }
    fn set_attributes_impl(
        &mut self,
        _: &vfsi_sync::SetAttrsOp<vfsi_sync::Target<'_, FileOperand>>,
    ) -> vfsi_sync::VfResult<()> {
        Err(VfError::unsupported(0))
    }
}

impl vfsi_sync::backend::VectorBackend for ListingBackend {
    fn vgetattrs_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_sync::VecResult<()> {
        let requested = attrs.len();
        for (index, attrs) in attrs.iter_mut().enumerate() {
            let missing = if index >= self.missing_after {
                self.missing
            } else {
                AttrMask::empty()
            };
            attrs.returned = attrs.masks & !missing;
            attrs.ftype = vfsi_sync::VfType::Regular;
            attrs.mode = libc::S_IFREG | 0o644;
            attrs.size = 7;
            attrs.nlink = 1;
            attrs.fileid = 2;
            attrs.blocks = 1;
            attrs.uid = 0;
            attrs.gid = 0;
            attrs.rdev = 0x1234;
        }
        Ok(vec![Ok(()); requested])
    }

    fn vgetattrs_nofollow_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_sync::VecResult<()> {
        let count = attrs.len();
        let outcomes = self.vgetattrs_impl(attrs)?;
        for attrs in attrs.iter_mut() {
            attrs.ftype = vfsi_sync::VfType::Directory;
            attrs.mode = libc::S_IFDIR | 0o755;
        }
        self.emitted += count;
        Ok(outcomes)
    }

    fn listdir_impl(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> vfsi_sync::VfResult<Vec<VfAttrs>> {
        if recursive {
            return Err(VfError::unsupported(0));
        }
        let count = if max_count == 0 {
            self.entries_per_dir
        } else {
            self.entries_per_dir.min(max_count)
        };
        let mut entries = Vec::with_capacity(count);
        for entry in 0..count {
            let returned = if self.emitted >= self.missing_after {
                masks & !self.missing
            } else {
                masks
            };
            entries.push(VfAttrs {
                file: match self.malformed_reply {
                    Some(MalformedReply::ListingNulName) => {
                        FileOperand::from_os_path(&dir.join("bad\0name"))
                    }
                    Some(MalformedReply::ListingMissingName) => FileOperand::from_fd(10),
                    _ => FileOperand::from_os_path(&dir.join(format!("entry-{entry}"))),
                },
                masks,
                returned,
                ftype: vfsi_sync::VfType::Regular,
                mode: libc::S_IFREG | 0o644,
                uid: 0,
                gid: 0,
                rdev: 0x1234,
                ..Default::default()
            });
            self.emitted += 1;
        }
        Ok(entries)
    }

    fn vopen_raw_impl(
        &mut self,
        _: &[&Path],
        _: &[i32],
        _: &[u32],
    ) -> vfsi_sync::VecResult<FileOperand> {
        let count = self
            .open_result_count
            .ok_or_else(|| VfError::unsupported(0))?;
        let results: Vec<FileOperand> = match self.malformed_reply {
            Some(MalformedReply::OpenDuplicate) => vec![FileOperand::from_fd(10_000); count],
            Some(MalformedReply::OpenPath) => (0..count)
                .map(|index| {
                    if index + 1 == count {
                        FileOperand::from_path("/not-an-open-handle")
                    } else {
                        FileOperand::from_fd(10_000 + index as i32)
                    }
                })
                .collect(),
            Some(MalformedReply::OpenNegativeFd) => vec![FileOperand::from_fd(-1); count],
            _ => (0..count)
                .map(|index| FileOperand::from_fd(10_000 + index as i32))
                .collect(),
        };
        Ok(results.into_iter().map(Ok).collect())
    }

    fn vopen_preflight_impl(&mut self, _: &[&Path], _: &[i32], _: &[u32]) -> vfsi_sync::VfRes {
        match self.open_preflight_error {
            Some(index) => Err(VfError::unsupported(index)),
            None => Ok(()),
        }
    }

    fn vread_impl(&mut self, reads: &[ReadOp]) -> vfsi_sync::VecResult<vfsi_sync::ReadResult> {
        if self.malformed_reply == Some(MalformedReply::ReadPanic) {
            panic!("injected backend panic");
        }

        let result: vfsi_sync::VfResult<Vec<vfsi_sync::ReadResult>> = {
            let Some(size) = self.read_size else {
                return Err(VfError::unsupported(0));
            };
            let result_count = self.read_result_count.unwrap_or(reads.len());
            Ok(reads
                .iter()
                .cycle()
                .take(result_count)
                .map(|read| vfsi_sync::ReadResult {
                    file: if self.malformed_reply == Some(MalformedReply::ReadWrongFile) {
                        FileOperand::from_fd(999)
                    } else {
                        read.file.clone()
                    },
                    offset: if self.malformed_reply == Some(MalformedReply::ReadWrongOffset) {
                        1
                    } else {
                        match read.offset {
                            vfsi_sync::VfOffset::At(offset) => offset,
                            _ => 0,
                        }
                    },
                    data: if self.malformed_reply == Some(MalformedReply::ReadNoProgress) {
                        Vec::new()
                    } else {
                        vec![b'x'; size]
                    },
                    eof: false,
                })
                .collect())
        };
        result.map(|results| {
            results
                .into_iter()
                .enumerate()
                .map(|(index, read)| {
                    if index == 1
                        && self.malformed_reply == Some(MalformedReply::ReadMiddleTransport)
                    {
                        Err(vfsi_sync::OpError::unknown(
                            VfError::transport(None, "lost read"),
                            Some(vfsi_sync::IoProgress::Read(
                                vfsi_sync::api::ReadResult::owned(
                                    read.offset,
                                    b"p".to_vec(),
                                    false,
                                ),
                            )),
                        ))
                    } else {
                        Ok(read)
                    }
                })
                .collect()
        })
    }

    fn vwrite_impl(
        &mut self,
        writes: &[WriteOp<&FileOperand, &[u8]>],
    ) -> vfsi_sync::VecResult<vfsi_sync::WriteResult> {
        let calls = self
            .write_calls
            .as_ref()
            .ok_or_else(|| VfError::unsupported(0))?;
        calls.lock().unwrap().push(
            writes
                .iter()
                .map(|op| (op.data().as_ptr() as usize, op.data().len()))
                .collect(),
        );
        let result_count = self.write_result_count.unwrap_or(writes.len());
        Ok(writes
            .iter()
            .cycle()
            .take(result_count)
            .map(|op| vfsi_sync::WriteResult {
                file: if self.malformed_reply == Some(MalformedReply::WriteWrongFile) {
                    FileOperand::from_fd(999)
                } else {
                    op.file().clone()
                },
                offset: match op.offset() {
                    vfsi_sync::VfOffset::At(offset)
                        if self.malformed_reply == Some(MalformedReply::WriteWrongOffset) =>
                    {
                        offset + 1
                    }
                    vfsi_sync::VfOffset::At(offset) => offset,
                    _ => unreachable!(),
                },
                bytes_written: if self.malformed_reply == Some(MalformedReply::WriteTooManyBytes) {
                    op.data().len() + 1
                } else {
                    op.data().len()
                },
                stable: true,
            })
            .enumerate()
            .map(|(index, write)| {
                if index == 1 && self.malformed_reply == Some(MalformedReply::WriteMiddlePartial) {
                    Err(vfsi_sync::OpError::unknown(
                        VfError::transport(None, "lost write"),
                        Some(vfsi_sync::IoProgress::Write { bytes_written: 1 }),
                    ))
                } else {
                    Ok(write)
                }
            })
            .collect())
    }

    fn vlistdirs_impl(
        &mut self,
        dirs: &[&Path],
        masks: AttrMask,
        max_entries: usize,
        recursive: bool,
        cb: &mut dyn FnMut(&VfAttrs, &Path) -> bool,
    ) -> vfsi_sync::VfRes {
        assert!(!recursive);
        let call = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(ListingCall {
                directories: dirs.iter().map(|path| path.to_path_buf()).collect(),
                max_entries,
            });
            calls.len()
        };
        if self.fail_call == Some(call) {
            return Err(VfError::errno(0, libc::EIO as u32));
        }
        let mut delivered = 0;
        for dir in dirs {
            for entry in 0..self.entries_per_dir {
                if max_entries != 0 && delivered >= max_entries {
                    return Ok(());
                }
                let returned = if self.emitted >= self.missing_after {
                    masks & !self.missing
                } else {
                    masks
                };
                let attrs = VfAttrs {
                    file: match self.malformed_reply {
                        Some(MalformedReply::ListingNulName) => {
                            FileOperand::from_os_path(&dir.join("bad\0name"))
                        }
                        Some(MalformedReply::ListingMissingName) => FileOperand::from_fd(10),
                        _ => FileOperand::from_os_path(&dir.join(format!("entry-{entry}"))),
                    },
                    masks,
                    returned,
                    mode: libc::S_IFREG | 0o644,
                    // Zero ownership is valid when explicitly returned.
                    uid: 0,
                    gid: 0,
                    rdev: 0x1234,
                    ..Default::default()
                };
                self.emitted += 1;
                let parent = if self.malformed_reply == Some(MalformedReply::ListingNulDirectory) {
                    dir.join("bad\0directory")
                } else {
                    dir.to_path_buf()
                };
                if !cb(&attrs, &parent) {
                    return Ok(());
                }
                delivered += 1;
            }
        }
        Ok(())
    }
}
impl BindingBackend for ListingBackend {}

struct ListingFixture {
    fs: *mut vfsi_fs,
    calls: std::sync::Arc<Mutex<Vec<ListingCall>>>,
}
impl ListingFixture {
    fn new(entries_per_dir: usize, missing: AttrMask, missing_after: usize) -> Self {
        Self::with_failure(entries_per_dir, missing, missing_after, None, None)
    }

    fn with_failure(
        entries_per_dir: usize,
        missing: AttrMask,
        missing_after: usize,
        fail_call: Option<usize>,
        open_preflight_error: Option<usize>,
    ) -> Self {
        let calls = std::sync::Arc::new(Mutex::new(Vec::new()));
        Self {
            fs: make_fs(
                Box::new(ListingBackend {
                    calls: calls.clone(),
                    entries_per_dir,
                    emitted: 0,
                    missing,
                    missing_after,
                    fail_call,
                    write_calls: None,
                    open_preflight_error,
                    read_size: None,
                    open_result_count: None,
                    read_result_count: None,
                    write_result_count: None,
                    close_attempts: None,
                    close_failure_fd: None,
                    malformed_reply: None,
                }),
                PathBuf::from("/"),
                PathBuf::from("/"),
            ),
            calls,
        }
    }

    fn run(
        &self,
        dirs: &[CString],
        options: &vfsi_listing_options,
    ) -> (c_int, Vec<usize>, Vec<vfsi_listing_result>) {
        unsafe extern "C" fn cb(
            index: usize,
            _: *const c_char,
            _: *const c_char,
            _: *const vfsi_attrs,
            user: *mut c_void,
        ) -> bool {
            let indices = &mut *(user as *mut Vec<usize>);
            indices.push(index);
            true
        }
        let pointers: Vec<_> = dirs.iter().map(|dir| dir.as_ptr()).collect();
        // Failure must clear even caller-supplied, previously successful results.
        let mut results = vec![
            vfsi_listing_result {
                completion: 1,
                delivered: 7
            };
            dirs.len()
        ];
        let mut indices = Vec::<usize>::new();
        let status = unsafe {
            vfsi_listdirs(
                self.fs,
                pointers.as_ptr(),
                pointers.len(),
                options,
                Some(cb),
                &mut indices as *mut _ as *mut c_void,
                results.as_mut_ptr(),
            )
        };
        (status, indices, results)
    }
}

#[test]
fn openv_preflight_failure_preserves_not_attempted_items() {
    let fixture = ListingFixture::with_failure(0, AttrMask::empty(), 0, None, Some(1));
    let first = CString::new("/first").unwrap();
    let second = CString::new("/second").unwrap();
    let mut ops = [
        vfsi_open_op {
            path: first.as_ptr(),
            flags: libc::O_CREAT | libc::O_RDWR,
            mode: 0o666,
            fd: -1,
        },
        vfsi_open_op {
            path: second.as_ptr(),
            flags: libc::O_CREAT | libc::O_RDWR,
            mode: 0o600,
            fd: -1,
        },
    ];
    let mut items = [vfsi_result::panic(); 2];
    let result = unsafe { vfsi_openv(fixture.fs, ops.as_mut_ptr(), ops.len(), items.as_mut_ptr()) };

    assert_eq!(result.index, 1);
    assert_eq!(result.err_no, VF_ERR_UNSUPPORTED);
    assert_eq!(items[0].category, VFSI_ERROR_NOT_ATTEMPTED);
    assert_eq!(items[1].category, VFSI_ERROR_UNSUPPORTED);
    assert_eq!(items[1].err_no, VF_ERR_UNSUPPORTED);
    assert_eq!(items[1].index, 1);
}

#[test]
fn c_openv_rejects_backend_result_cardinality_mismatch_and_cleans_handles() {
    for returned in [1, 3] {
        let close_attempts = std::sync::Arc::new(Mutex::new(Vec::new()));
        let fs = make_fs(
            Box::new(ListingBackend {
                calls: Default::default(),
                entries_per_dir: 0,
                emitted: 0,
                missing: AttrMask::empty(),
                missing_after: 0,
                fail_call: None,
                write_calls: None,
                open_preflight_error: None,
                read_size: None,
                open_result_count: Some(returned),
                read_result_count: None,
                write_result_count: None,
                close_attempts: Some(close_attempts.clone()),
                close_failure_fd: if returned > 1 { Some(10_000) } else { None },
                malformed_reply: None,
            }),
            PathBuf::from("/"),
            PathBuf::from("/"),
        );
        let first = CString::new("/first").unwrap();
        let second = CString::new("/second").unwrap();
        let mut ops = [
            vfsi_open_op {
                path: first.as_ptr(),
                flags: libc::O_RDONLY,
                mode: 0,
                fd: -1,
            },
            vfsi_open_op {
                path: second.as_ptr(),
                flags: libc::O_RDONLY,
                mode: 0,
                fd: -1,
            },
        ];
        let mut items = [vfsi_result::success(0); 2];

        let result = unsafe { vfsi_openv(fs, ops.as_mut_ptr(), ops.len(), items.as_mut_ptr()) };

        assert_eq!(result.category, VFSI_ERROR_TRANSPORT);
        assert_eq!(result.err_no, vfsi_sync::VF_ERR_RPC);
        assert!(items
            .iter()
            .all(|item| item.category == VFSI_ERROR_INDETERMINATE));
        assert_eq!(ops.map(|op| op.fd), [-1, -1]);
        assert!(unsafe { &*fs }.files.lock().unwrap().is_empty());
        assert_eq!(
            *close_attempts.lock().unwrap(),
            (0..returned)
                .map(|index| 10_000 + index as i32)
                .collect::<Vec<_>>()
        );
        unsafe { vfsi_free(fs) };
    }
}

#[test]
fn c_openv_rejects_duplicate_and_non_handle_results() {
    for (malformed_reply, expected_closes) in [
        (MalformedReply::OpenDuplicate, vec![10_000]),
        (MalformedReply::OpenPath, vec![10_000]),
        (MalformedReply::OpenNegativeFd, vec![]),
    ] {
        let close_attempts = std::sync::Arc::new(Mutex::new(Vec::new()));
        let fs = make_fs(
            Box::new(ListingBackend {
                calls: Default::default(),
                entries_per_dir: 0,
                emitted: 0,
                missing: AttrMask::empty(),
                missing_after: 0,
                fail_call: None,
                write_calls: None,
                open_preflight_error: None,
                read_size: None,
                open_result_count: Some(2),
                read_result_count: None,
                write_result_count: None,
                close_attempts: Some(close_attempts.clone()),
                close_failure_fd: None,
                malformed_reply: Some(malformed_reply),
            }),
            PathBuf::from("/"),
            PathBuf::from("/"),
        );
        let first = CString::new("/first").unwrap();
        let second = CString::new("/second").unwrap();
        let mut ops = [
            vfsi_open_op {
                path: first.as_ptr(),
                flags: libc::O_RDONLY,
                mode: 0,
                fd: -1,
            },
            vfsi_open_op {
                path: second.as_ptr(),
                flags: libc::O_RDONLY,
                mode: 0,
                fd: -1,
            },
        ];
        let mut items = [vfsi_result::success(0); 2];

        let result = unsafe { vfsi_openv(fs, ops.as_mut_ptr(), ops.len(), items.as_mut_ptr()) };

        assert_eq!(result.category, VFSI_ERROR_TRANSPORT);
        assert!(items
            .iter()
            .all(|item| item.category == VFSI_ERROR_INDETERMINATE));
        assert_eq!(ops.map(|op| op.fd), [-1, -1]);
        assert!(unsafe { &*fs }.files.lock().unwrap().is_empty());
        assert_eq!(*close_attempts.lock().unwrap(), expected_closes);
        unsafe { vfsi_free(fs) };
    }
}

#[test]
fn c_vector_reads_reject_backend_result_cardinality_mismatch_before_copying() {
    for returned in [0, 2] {
        let fs = make_fs(
            Box::new(ListingBackend {
                calls: Default::default(),
                entries_per_dir: 0,
                emitted: 0,
                missing: AttrMask::empty(),
                missing_after: 0,
                fail_call: None,
                write_calls: None,
                open_preflight_error: None,
                read_size: Some(1),
                open_result_count: None,
                read_result_count: Some(returned),
                write_result_count: None,
                close_attempts: None,
                close_failure_fd: None,
                malformed_reply: None,
            }),
            PathBuf::from("/"),
            PathBuf::from("/"),
        );
        unsafe { &*fs }
            .files
            .lock()
            .unwrap()
            .insert(10, FileOperand::from_fd(100).into());
        let mut bytes = [0xa5u8; 4];
        let mut op = vfsi_pread_op {
            fd: 10,
            buf: bytes.as_mut_ptr().cast(),
            len: bytes.len(),
            offset: 0,
            got: 99,
        };
        let mut item = vfsi_result::success(0);

        let result = unsafe { vfsi_preadv(fs, &mut op, 1, &mut item) };

        assert_eq!(result.category, VFSI_ERROR_TRANSPORT);
        assert_eq!(item.category, VFSI_ERROR_INDETERMINATE);
        assert_eq!(op.got, 99);
        assert_eq!(bytes, [0xa5; 4]);
        unsafe { vfsi_free(fs) };
    }
}

#[test]
fn c_vector_writes_reject_backend_result_cardinality_mismatch() {
    for returned in [0, 2] {
        let calls = std::sync::Arc::new(Mutex::new(Vec::new()));
        let fs = make_fs(
            Box::new(ListingBackend {
                calls: Default::default(),
                entries_per_dir: 0,
                emitted: 0,
                missing: AttrMask::empty(),
                missing_after: 0,
                fail_call: None,
                write_calls: Some(calls.clone()),
                open_preflight_error: None,
                read_size: None,
                open_result_count: None,
                read_result_count: None,
                write_result_count: Some(returned),
                close_attempts: None,
                close_failure_fd: None,
                malformed_reply: None,
            }),
            PathBuf::from("/"),
            PathBuf::from("/"),
        );
        unsafe { &*fs }
            .files
            .lock()
            .unwrap()
            .insert(10, FileOperand::from_fd(100).into());
        let data = b"payload";
        let mut op = vfsi_pwrite_op {
            fd: 10,
            buf: data.as_ptr().cast(),
            len: data.len(),
            offset: 0,
            wrote: 99,
        };
        let mut item = vfsi_result::success(0);

        let result = unsafe { vfsi_pwritev(fs, &mut op, 1, &mut item) };

        assert_eq!(result.category, VFSI_ERROR_TRANSPORT);
        assert_eq!(item.category, VFSI_ERROR_INDETERMINATE);
        assert_eq!(op.wrote, 99);
        assert_eq!(calls.lock().unwrap().len(), 1);
        unsafe { vfsi_free(fs) };
    }
}

#[test]
fn cancelled_stream_reports_only_delivered_file_completions() {
    struct Seen {
        calls: usize,
        stop_after: usize,
        completed: usize,
    }
    unsafe extern "C" fn callback(
        _: *const c_char,
        _: usize,
        _: u64,
        _: *const u8,
        _: usize,
        eof: bool,
        userdata: *mut c_void,
    ) -> bool {
        let seen = &mut *userdata.cast::<Seen>();
        seen.calls += 1;
        seen.completed += usize::from(eof);
        seen.calls < seen.stop_after
    }
    for (first, stop_after, completed) in [
        (b"abcdef".as_slice(), 1, 0),
        // A positive short local read is progress, not proven EOF.
        (b"a".as_slice(), 1, 0),
        (b"".as_slice(), 1, 1),
        (b"a".as_slice(), 2, 0),
        (b"a".as_slice(), 3, 1),
        (b"abcdef".as_slice(), usize::MAX, 2),
    ] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("first"), first).unwrap();
        std::fs::write(root.path().join("second"), b"second-file").unwrap();
        let fs = make_fs(
            Box::new(vfsi_posix::backend(root.path()).unwrap()),
            PathBuf::from("/"),
            PathBuf::from("/"),
        );
        let paths = [c"/first".as_ptr(), c"/second".as_ptr()];
        let mut seen = Seen {
            calls: 0,
            stop_after,
            completed: 0,
        };
        let result = unsafe {
            vfsi_read_streamv(
                fs,
                paths.as_ptr(),
                paths.len(),
                3,
                3,
                Some(callback),
                (&mut seen as *mut Seen).cast(),
            )
        };
        assert_eq!(result.category, VFSI_ERROR_NONE);
        assert_eq!(seen.completed, completed);
        assert_eq!(result.index, completed);
        if stop_after != usize::MAX {
            assert_eq!(seen.calls, stop_after);
        }
        unsafe { vfsi_free(fs) };
    }
}

#[test]
fn stream_callbacks_can_query_same_filesystem_without_lock() {
    struct Seen {
        fs: *mut vfsi_fs,
        available: bool,
        calls: usize,
        data: [Vec<u8>; 2],
    }
    unsafe extern "C" fn callback(
        _: *const c_char,
        index: usize,
        _: u64,
        data: *const u8,
        len: usize,
        _: bool,
        userdata: *mut c_void,
    ) -> bool {
        // SAFETY: the fixture owns the state and keeps the filesystem alive.
        let seen = unsafe { &mut *userdata.cast::<Seen>() };
        seen.calls += 1;
        // Fail promptly on the old behavior rather than deadlocking the test.
        let available = match unsafe { &*seen.fs }.fs.try_lock() {
            Ok(guard) => {
                drop(guard);
                true
            }
            Err(_) => false,
        };
        if !available {
            seen.available = false;
            return false;
        }
        let mut attrs = std::mem::MaybeUninit::<vfsi_attrs>::uninit();
        // SAFETY: this nested operation borrows the same live handle and
        // exclusively owns its output; no outer mutable buffer is aliased.
        if unsafe { vfsi_stat(seen.fs, c"/other".as_ptr(), attrs.as_mut_ptr()) } != 0 {
            seen.available = false;
            return false;
        }
        let Some(output) = seen.data.get_mut(index) else {
            seen.available = false;
            return false;
        };
        // SAFETY: the streaming callback lends len initialized bytes.
        output.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
        true
    }
    let root = tempfile::tempdir().unwrap();
    for (name, bytes) in [
        ("first", b"abcdef".as_slice()),
        ("second", b"xyzq"),
        ("other", b"stat"),
    ] {
        std::fs::write(root.path().join(name), bytes).unwrap();
    }
    let fs = make_fs(
        Box::new(vfsi_posix::backend(root.path()).unwrap()),
        PathBuf::from("/"),
        PathBuf::from("/"),
    );
    let mut seen = Seen {
        fs,
        available: true,
        calls: 0,
        data: Default::default(),
    };
    let paths = [c"/first".as_ptr(), c"/second".as_ptr()];
    // SAFETY: all inputs, outputs and callback state outlive this call.
    let result = unsafe {
        vfsi_read_streamv(
            fs,
            paths.as_ptr(),
            paths.len(),
            3,
            6,
            Some(callback),
            &mut seen as *mut _ as *mut c_void,
        )
    };
    assert!(seen.available, "stream callback ran under the backend lock");
    assert!(seen.calls > 2);
    assert_eq!((result.category, result.index), (VFSI_ERROR_NONE, 2));
    assert_eq!(seen.data, [b"abcdef".to_vec(), b"xyzq".to_vec()]);
    // SAFETY: the fixture uniquely owns the live handle.
    unsafe { vfsi_free(fs) };
}

#[test]
fn c_scalar_writes_preserve_append_and_positional_handles() {
    let root = tempfile::tempdir().unwrap();
    let raw = make_fs(
        Box::new(vfsi_posix::backend(root.path()).unwrap()),
        PathBuf::from("/"),
        PathBuf::from("/"),
    );
    for (path, append) in [(c"/append", true), (c"/positional", false)] {
        let fd = unsafe {
            vfsi_open(
                raw,
                path.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | if append { libc::O_APPEND } else { 0 },
                0o644,
            )
        };
        assert!(fd >= 0);
        for (payload, offset) in [(b"abc".as_slice(), 0), (b"XY".as_slice(), 1)] {
            let mut wrote = usize::MAX;
            assert_eq!(
                unsafe {
                    vfsi_pwrite(
                        raw,
                        fd,
                        payload.as_ptr().cast(),
                        payload.len(),
                        offset,
                        &mut wrote,
                    )
                },
                0
            );
            assert_eq!(wrote, payload.len());
        }
        assert_eq!(unsafe { vfsi_close(raw, fd) }, 0);
    }
    unsafe { vfsi_free(raw) };
    assert_eq!(std::fs::read(root.path().join("append")).unwrap(), b"abcXY");
    assert_eq!(
        std::fs::read(root.path().join("positional")).unwrap(),
        b"aXY"
    );
}

#[test]
fn c_vector_writes_preserve_append_and_positional_handles() {
    for vector_open in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let raw = make_fs(
            Box::new(vfsi_posix::backend(root.path()).unwrap()),
            PathBuf::from("/"),
            PathBuf::from("/"),
        );
        let append = CString::new("/append").unwrap();
        let positional = CString::new("/positional").unwrap();
        let mut opens = [
            vfsi_open_op {
                path: append.as_ptr(),
                flags: libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
                mode: 0o644,
                fd: -1,
            },
            vfsi_open_op {
                path: positional.as_ptr(),
                flags: libc::O_WRONLY | libc::O_CREAT,
                mode: 0o644,
                fd: -1,
            },
        ];
        if vector_open {
            let mut items = [vfsi_result::panic(); 2];
            assert_eq!(
                unsafe { vfsi_openv(raw, opens.as_mut_ptr(), 2, items.as_mut_ptr()) }.category,
                VFSI_ERROR_NONE
            );
        } else {
            for op in &mut opens {
                op.fd = unsafe { vfsi_open(raw, op.path, op.flags, op.mode) };
                assert!(op.fd >= 0);
            }
        }
        for (payload, offset) in [(b"abc".as_slice(), 0), (b"XY".as_slice(), 1)] {
            let mut writes = opens.map(|op| vfsi_pwrite_op {
                fd: op.fd,
                buf: payload.as_ptr().cast(),
                len: payload.len(),
                offset,
                wrote: 99,
            });
            let mut items = [vfsi_result::panic(); 2];
            let result = unsafe { vfsi_pwritev(raw, writes.as_mut_ptr(), 2, items.as_mut_ptr()) };
            assert_eq!(result.category, VFSI_ERROR_NONE, "{}", result.err_no);
            assert!(items.iter().all(|item| item.category == VFSI_ERROR_NONE));
            assert!(writes.iter().all(|op| op.wrote == payload.len()));
        }
        unsafe { vfsi_free(raw) };
        assert_eq!(std::fs::read(root.path().join("append")).unwrap(), b"abcXY");
        assert_eq!(
            std::fs::read(root.path().join("positional")).unwrap(),
            b"aXY"
        );
    }
}

#[test]
fn result_arrays_initialize_uninitialized_foreign_storage() {
    let mut output = [std::mem::MaybeUninit::<vfsi_result>::uninit(); 2];
    let results = unsafe { result_array(output.as_mut_ptr().cast(), output.len()) }
        .unwrap_or_else(|_| panic!("valid writable output array"));
    for (index, result) in results.iter().enumerate() {
        assert_eq!(result.index, index);
        assert_eq!(result.category, VFSI_ERROR_NOT_ATTEMPTED);
    }
}

#[test]
fn c_vector_reads_never_copy_beyond_the_requested_output_length() {
    for returned in [2, 3] {
        let fixture = ListingFixture {
            fs: make_fs(
                Box::new(ListingBackend {
                    calls: Default::default(),
                    entries_per_dir: 0,
                    emitted: 0,
                    missing: AttrMask::empty(),
                    missing_after: 0,
                    fail_call: None,
                    write_calls: None,
                    open_preflight_error: None,
                    read_size: Some(returned),
                    open_result_count: None,
                    read_result_count: None,
                    write_result_count: None,
                    close_attempts: None,
                    close_failure_fd: None,
                    malformed_reply: None,
                }),
                PathBuf::from("/"),
                PathBuf::from("/"),
            ),
            calls: Default::default(),
        };
        unsafe { &*fixture.fs }
            .files
            .lock()
            .unwrap()
            .insert(10, FileOperand::from_fd(100).into());
        // Keep all canaries inside one real allocation, so even an old
        // overlong copy can be detected without writing out of allocation.
        let mut bytes = [0xa5u8; 8];
        let mut operation = vfsi_pread_op {
            fd: 10,
            buf: bytes.as_mut_ptr().wrapping_add(2).cast(),
            len: 2,
            offset: 0,
            got: 99,
        };
        let mut item = vfsi_result::panic();
        let status = unsafe { vfsi_preadv(fixture.fs, &mut operation, 1, &mut item) };
        if returned == 2 {
            assert_eq!(status.category, VFSI_ERROR_NONE);
            assert_eq!(operation.got, 2);
            assert_eq!(bytes, [0xa5, 0xa5, b'x', b'x', 0xa5, 0xa5, 0xa5, 0xa5]);
        } else {
            assert_eq!(status.category, VFSI_ERROR_TRANSPORT);
            assert_eq!(item.category, VFSI_ERROR_INDETERMINATE);
            assert_eq!(operation.got, 99);
            assert_eq!(bytes, [0xa5; 8]);
        }
    }
}

#[test]
fn c_vector_reads_reject_malformed_backend_results_before_copying() {
    for malformed_reply in [
        MalformedReply::ReadWrongFile,
        MalformedReply::ReadWrongOffset,
        MalformedReply::ReadNoProgress,
    ] {
        let fs = make_fs(
            Box::new(ListingBackend {
                calls: Default::default(),
                entries_per_dir: 0,
                emitted: 0,
                missing: AttrMask::empty(),
                missing_after: 0,
                fail_call: None,
                write_calls: None,
                open_preflight_error: None,
                read_size: Some(1),
                open_result_count: None,
                read_result_count: None,
                write_result_count: None,
                close_attempts: None,
                close_failure_fd: None,
                malformed_reply: Some(malformed_reply),
            }),
            PathBuf::from("/"),
            PathBuf::from("/"),
        );
        unsafe { &*fs }
            .files
            .lock()
            .unwrap()
            .insert(10, FileOperand::from_fd(100).into());
        let mut bytes = [0xa5u8; 4];
        let mut op = vfsi_pread_op {
            fd: 10,
            buf: bytes.as_mut_ptr().cast(),
            len: bytes.len(),
            offset: 7,
            got: 99,
        };
        let mut item = vfsi_result::success(0);

        let result = unsafe { vfsi_preadv(fs, &mut op, 1, &mut item) };

        assert_eq!(result.category, VFSI_ERROR_TRANSPORT);
        assert_eq!(item.category, VFSI_ERROR_INDETERMINATE);
        assert_eq!(op.got, 99);
        assert_eq!(bytes, [0xa5; 4]);
        unsafe { vfsi_free(fs) };
    }
}

#[test]
fn c_vector_writes_reject_malformed_backend_results() {
    for malformed_reply in [
        MalformedReply::WriteWrongFile,
        MalformedReply::WriteWrongOffset,
        MalformedReply::WriteTooManyBytes,
    ] {
        let calls = std::sync::Arc::new(Mutex::new(Vec::new()));
        let fs = make_fs(
            Box::new(ListingBackend {
                calls: Default::default(),
                entries_per_dir: 0,
                emitted: 0,
                missing: AttrMask::empty(),
                missing_after: 0,
                fail_call: None,
                write_calls: Some(calls.clone()),
                open_preflight_error: None,
                read_size: None,
                open_result_count: None,
                read_result_count: None,
                write_result_count: None,
                close_attempts: None,
                close_failure_fd: None,
                malformed_reply: Some(malformed_reply),
            }),
            PathBuf::from("/"),
            PathBuf::from("/"),
        );
        unsafe { &*fs }
            .files
            .lock()
            .unwrap()
            .insert(10, FileOperand::from_fd(100).into());
        let data = b"payload";
        let mut op = vfsi_pwrite_op {
            fd: 10,
            buf: data.as_ptr().cast(),
            len: data.len(),
            offset: 7,
            wrote: 99,
        };
        let mut item = vfsi_result::success(0);

        let result = unsafe { vfsi_pwritev(fs, &mut op, 1, &mut item) };

        assert_eq!(result.category, VFSI_ERROR_TRANSPORT);
        assert_eq!(item.category, VFSI_ERROR_INDETERMINATE);
        assert_eq!(op.wrote, 99);
        assert_eq!(calls.lock().unwrap().len(), 1);
        unsafe { vfsi_free(fs) };
    }
}

#[test]
fn c_vector_writes_borrow_payloads_and_preflight_the_entire_batch() {
    let calls = std::sync::Arc::new(Mutex::new(Vec::new()));
    let fs = make_fs(
        Box::new(ListingBackend {
            calls: Default::default(),
            entries_per_dir: 0,
            emitted: 0,
            missing: AttrMask::empty(),
            missing_after: 0,
            fail_call: None,
            write_calls: Some(calls.clone()),
            open_preflight_error: None,
            read_size: None,
            open_result_count: None,
            read_result_count: None,
            write_result_count: None,
            close_attempts: None,
            close_failure_fd: None,
            malformed_reply: None,
        }),
        PathBuf::from("/"),
        PathBuf::from("/"),
    );
    let fs_ref = unsafe { &*fs };
    fs_ref
        .files
        .lock()
        .unwrap()
        .insert(10, FileOperand::from_fd(100).into());
    fs_ref
        .files
        .lock()
        .unwrap()
        .insert(11, FileOperand::from_fd(101).into());
    let data = b"payload owned by C caller";
    let mut ops = [
        vfsi_pwrite_op {
            fd: 10,
            buf: data.as_ptr().cast(),
            len: data.len(),
            offset: 7,
            wrote: 0,
        },
        vfsi_pwrite_op {
            fd: 11,
            buf: std::ptr::null(),
            len: 0,
            offset: 0,
            wrote: 0,
        },
    ];
    let mut results = [vfsi_result::success(0); 2];
    let result = unsafe { vfsi_pwritev(fs, ops.as_mut_ptr(), 2, results.as_mut_ptr()) };
    assert_eq!(result.category, VFSI_ERROR_NONE);
    assert_eq!(ops[0].wrote, data.len());
    assert_eq!(ops[1].wrote, 0);
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], (data.as_ptr() as usize, data.len()));
        assert_eq!(calls[0][1].1, 0);
    }
    // A later invalid input cannot cause an earlier valid write to run.
    ops[1].fd = 999;
    assert_ne!(
        unsafe { vfsi_pwritev(fs, ops.as_mut_ptr(), 2, results.as_mut_ptr()) }.category,
        VFSI_ERROR_NONE
    );
    ops[1].fd = 11;
    ops[1].len = 1;
    assert_ne!(
        unsafe { vfsi_pwritev(fs, ops.as_mut_ptr(), 2, results.as_mut_ptr()) }.category,
        VFSI_ERROR_NONE
    );
    assert_eq!(calls.lock().unwrap().len(), 1);
    unsafe {
        drop(Box::from_raw(fs));
    }
}
impl Drop for ListingFixture {
    fn drop(&mut self) {
        unsafe { vfsi_free(self.fs) };
    }
}

#[test]
fn c_metadata_apis_reject_missing_requested_fields_without_callbacks() {
    unsafe extern "C" fn count_listdir(
        _: *const c_char,
        _: *const vfsi_attrs,
        userdata: *mut c_void,
    ) -> bool {
        *(userdata as *mut usize) += 1;
        true
    }
    unsafe extern "C" fn count_listdirv(
        _: *const c_char,
        _: *const c_char,
        _: *const vfsi_attrs,
        userdata: *mut c_void,
    ) -> bool {
        *(userdata as *mut usize) += 1;
        true
    }
    let path = CString::new("/file").unwrap();
    let dir = CString::new("/dir").unwrap();
    let dirs = [dir.as_ptr()];

    for bit in mask().iter() {
        let fixture = ListingFixture::new(1, bit, 0);
        let mut attrs = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { vfsi_stat(fixture.fs, path.as_ptr(), &mut attrs) },
            libc::ENOTSUP,
            "scalar stat accepted missing {bit:?}"
        );

        let mut stale_attrs: vfsi_attrs = unsafe { std::mem::zeroed() };
        stale_attrs.size = 999;
        let mut op = vfsi_stat_op {
            path: path.as_ptr(),
            attrs: stale_attrs,
        };
        let mut item = [vfsi_result::panic()];
        let result = unsafe { vfsi_statv(fixture.fs, &mut op, 1, item.as_mut_ptr()) };
        assert_eq!(result.category, VFSI_ERROR_UNSUPPORTED, "{bit:?}");
        assert_eq!(result.err_no, libc::ENOTSUP as u32, "{bit:?}");
        assert_eq!(item[0].category, VFSI_ERROR_UNSUPPORTED, "{bit:?}");
        assert_eq!(
            op.attrs.struct_size,
            std::mem::size_of::<vfsi_attrs>() as u32
        );
        assert_eq!(op.attrs.size, 0, "failed statv must clear stale output");
    }

    for bit in mask().iter() {
        let mut calls = 0usize;
        let listdir_fixture = ListingFixture::new(2, bit, 1);
        assert_eq!(
            unsafe {
                vfsi_listdir_with_limits(
                    listdir_fixture.fs,
                    dir.as_ptr(),
                    10,
                    1024,
                    Some(count_listdir),
                    (&mut calls as *mut usize).cast(),
                )
            },
            libc::ENOTSUP,
            "listdir accepted missing {bit:?}"
        );
        assert_eq!(calls, 0, "listdir callback ran with missing {bit:?}");

        assert_eq!(
            unsafe {
                vfsi_listdirv(
                    ListingFixture::new(2, bit, 1).fs,
                    dirs.as_ptr(),
                    dirs.len(),
                    10,
                    false,
                    Some(count_listdirv),
                    (&mut calls as *mut usize).cast(),
                )
            },
            libc::ENOTSUP,
            "listdirv accepted missing {bit:?}"
        );
        assert_eq!(calls, 0, "listdirv callback ran with missing {bit:?}");
    }

    let mixed = ListingFixture::new(0, AttrMask::SIZE, 1);
    let mut ops = [
        vfsi_stat_op {
            path: path.as_ptr(),
            attrs: unsafe { std::mem::zeroed() },
        },
        vfsi_stat_op {
            path: path.as_ptr(),
            attrs: unsafe { std::mem::zeroed() },
        },
    ];
    let mut items = [vfsi_result::panic(); 2];
    let result = unsafe { vfsi_statv(mixed.fs, ops.as_mut_ptr(), ops.len(), items.as_mut_ptr()) };
    assert_eq!(result.index, 1);
    assert_eq!(result.category, VFSI_ERROR_UNSUPPORTED);
    assert_eq!(items[0].category, VFSI_ERROR_NONE);
    assert_eq!(items[1].category, VFSI_ERROR_UNSUPPORTED);
    assert_eq!(ops[0].attrs.size, 7);
    assert_eq!(ops[1].attrs.size, 0);

    let fixture = ListingFixture::new(1, AttrMask::empty(), 0);
    let mut attrs = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { vfsi_stat(fixture.fs, path.as_ptr(), &mut attrs) },
        0
    );
    assert_eq!(attrs.uid, 0, "explicitly returned zero is valid");
    assert_eq!(attrs.gid, 0, "explicitly returned zero is valid");
}

#[test]
fn c_attribute_options_reject_unrepresentable_rdev() {
    let metadata = vfsi_sync::metadata_from_attrs(VfAttrs {
        masks: AttrMask::RDEV,
        returned: AttrMask::RDEV,
        rdev: 0x1234,
        ..VfAttrs::default()
    });
    assert_eq!(metadata.device_id(), Some(0x1234));

    let dirs = [CString::new("/dir").unwrap()];
    let fixture = ListingFixture::new(1, AttrMask::empty(), 0);
    let options = vfsi_listing_options {
        max_entries: 10,
        max_path_bytes: 1024,
        attributes: VFSI_ATTR_RDEV,
    };
    let (status, indices, results) = fixture.run(&dirs, &options);
    assert_eq!(status, libc::ENOTSUP);
    assert!(indices.is_empty());
    assert!(results
        .iter()
        .all(|result| result.completion == 0 && result.delivered == 0));
    assert!(fixture.calls.lock().unwrap().is_empty());

    let root = CString::new("/").unwrap();
    let walk_options = vfsi_walk_options {
        max_entries: 10,
        max_path_bytes: 1024,
        max_depth: 10,
        attributes: VFSI_ATTR_RDEV,
        sort_by_name: 0,
    };
    unsafe extern "C" fn stop_walk(
        _: *const c_char,
        _: u32,
        _: usize,
        _: *const vfsi_attrs,
        _: *mut c_void,
    ) -> u32 {
        0
    }
    let mut calls = 0usize;
    assert_eq!(
        unsafe {
            vfsi_walk(
                fixture.fs,
                root.as_ptr(),
                &walk_options,
                Some(stop_walk),
                (&mut calls as *mut usize).cast(),
            )
        },
        libc::ENOTSUP
    );
    assert_eq!(calls, 0);
}

#[test]
fn c_walk_rejects_invalid_depth_before_backend_metadata() {
    unsafe extern "C" fn unexpected(
        _: *const c_char,
        _: u32,
        _: usize,
        _: *const vfsi_attrs,
        _: *mut c_void,
    ) -> u32 {
        panic!("callback must not run");
    }
    let root = CString::new("/").unwrap();
    // This backend would fail root metadata with ENOTSUP if called.
    let fixture = ListingFixture::new(1, AttrMask::MODE, 0);
    for max_depth in [65534, 65535, usize::MAX - 1] {
        let options = vfsi_walk_options {
            max_entries: 10,
            max_path_bytes: 1024,
            max_depth,
            attributes: VFSI_ATTR_MODE,
            sort_by_name: 0,
        };
        assert_eq!(
            unsafe {
                vfsi_walk(
                    fixture.fs,
                    root.as_ptr(),
                    &options,
                    Some(unexpected),
                    std::ptr::null_mut(),
                )
            },
            libc::EINVAL
        );
    }
}

#[test]
fn c_walk_checks_requested_metadata_before_callbacks() {
    unsafe extern "C" fn count_walk(
        _: *const c_char,
        kind: u32,
        _: usize,
        _: *const vfsi_attrs,
        userdata: *mut c_void,
    ) -> u32 {
        if kind == 1 {
            *(userdata as *mut usize) += 1;
        }
        0
    }
    let root = CString::new("/").unwrap();
    let fixture = ListingFixture::new(1, AttrMask::MODE, 0);
    let mut calls = 0usize;
    let options = vfsi_walk_options {
        max_entries: 10,
        max_path_bytes: 1024,
        max_depth: 10,
        attributes: VFSI_ATTR_MODE,
        sort_by_name: 0,
    };
    assert_eq!(
        unsafe {
            vfsi_walk(
                fixture.fs,
                root.as_ptr(),
                &options,
                Some(count_walk),
                (&mut calls as *mut usize).cast(),
            )
        },
        libc::ENOTSUP
    );
    assert_eq!(calls, 0, "walk callback ran with missing root mode");

    let fixture = ListingFixture::new(1, AttrMask::MODE, 1);
    assert_eq!(
        unsafe {
            vfsi_walk(
                fixture.fs,
                root.as_ptr(),
                &options,
                Some(count_walk),
                (&mut calls as *mut usize).cast(),
            )
        },
        libc::ENOTSUP
    );
    assert_eq!(calls, 0, "walk delivered entries with missing mode");
}

#[test]
fn indexed_listings_reject_missing_returned_fields_before_any_callback() {
    let options = vfsi_listing_options {
        max_entries: 10,
        max_path_bytes: 1024,
        attributes: VFSI_ATTR_SIZE
            | VFSI_ATTR_NLINK
            | VFSI_ATTR_FILEID
            | VFSI_ATTR_BLOCKS
            | VFSI_ATTR_UID
            | VFSI_ATTR_GID
            | VFSI_ATTR_ATIME
            | VFSI_ATTR_MTIME
            | VFSI_ATTR_CTIME,
    };
    let fields = AttrMask::from_bits(options.attributes).unwrap() | AttrMask::MODE;
    let dirs = [CString::new("/dir").unwrap()];
    for bit in fields.iter() {
        // A valid prefix followed by an omitted server field must not leak.
        let fixture = ListingFixture::new(2, bit, 1);
        let (status, indices, results) = fixture.run(&dirs, &options);
        assert_eq!(status, libc::ENOTSUP, "missing returned field: {bit:?}");
        assert!(indices.is_empty());
        assert!(results
            .iter()
            .all(|r| r.completion == 0 && r.delivered == 0));
    }
    let fixture = ListingFixture::new(2, AttrMask::empty(), 0);
    let (status, indices, results) = fixture.run(&dirs, &options);
    assert_eq!(status, 0, "explicitly returned zero ownership is valid");
    assert_eq!(indices, [0, 0]);
    assert_eq!(results[0].completion, 1);
    assert_eq!(results[0].delivered, 2);
}

#[test]
fn indexed_listings_bound_first_page_fetches_even_with_tiny_quotas() {
    let dirs: Vec<_> = (0..97)
        .map(|n| CString::new(format!("/dir-{n}")).unwrap())
        .collect();
    for (max_entries, max_path_bytes, expected_calls) in [
        (1, usize::MAX, 1),
        (usize::MAX, 1, 1),
        (32, usize::MAX, 2),
        (33, usize::MAX, 2),
    ] {
        let fixture = ListingFixture::new(1, AttrMask::empty(), 0);
        let (status, indices, results) = fixture.run(
            &dirs,
            &vfsi_listing_options {
                max_entries,
                max_path_bytes,
                attributes: VFSI_ATTR_MODE,
            },
        );
        assert_eq!(status, libc::EFBIG);
        assert!(indices.is_empty());
        assert!(results
            .iter()
            .all(|r| r.completion == 0 && r.delivered == 0));
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.len(), expected_calls);
        assert!(
            calls.iter().all(|call| call.directories.len() <= 32),
            "first pages must be bounded before backend callbacks"
        );
        if expected_calls == 2 {
            assert_eq!(
                calls[1].max_entries,
                max_entries - 32 + 1,
                "the next cohort must receive the remaining budget plus one probe"
            );
        }
    }
}

#[test]
fn indexed_listings_share_exact_budgets_across_cohorts_and_duplicate_operands() {
    let mut dirs: Vec<_> = (0..65)
        .map(|n| CString::new(format!("/dir-{n}")).unwrap())
        .collect();
    dirs.push(dirs[0].clone());
    let bytes = dirs
        .iter()
        .map(|dir| dir.as_bytes().len() + b"/entry-0".len())
        .sum();
    for (max_entries, max_path_bytes, status) in [
        (dirs.len(), bytes, 0),
        (dirs.len() - 1, bytes, libc::EFBIG),
        (dirs.len(), bytes - 1, libc::EFBIG),
    ] {
        let fixture = ListingFixture::new(1, AttrMask::empty(), 0);
        let (actual, mut indices, results) = fixture.run(
            &dirs,
            &vfsi_listing_options {
                max_entries,
                max_path_bytes,
                attributes: VFSI_ATTR_MODE,
            },
        );
        assert_eq!(actual, status);
        assert_eq!(fixture.calls.lock().unwrap().len(), 3);
        if status == 0 {
            indices.sort_unstable();
            assert_eq!(indices, (0..dirs.len()).collect::<Vec<_>>());
            assert!(results
                .iter()
                .all(|r| r.completion == 1 && r.delivered == 1));
        } else {
            assert!(
                indices.is_empty(),
                "a later-cohort overflow must not leak a prefix"
            );
            assert!(results
                .iter()
                .all(|r| r.completion == 0 && r.delivered == 0));
        }
    }
    // Zero response budgets still allow empty roots in multiple cohorts.
    let fixture = ListingFixture::new(0, AttrMask::empty(), 0);
    let (status, indices, results) = fixture.run(
        &dirs,
        &vfsi_listing_options {
            max_entries: 0,
            max_path_bytes: 0,
            attributes: VFSI_ATTR_MODE,
        },
    );
    assert_eq!(status, 0);
    assert!(indices.is_empty());
    assert!(results
        .iter()
        .all(|r| r.completion == 1 && r.delivered == 0));
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|call| call.max_entries == 1));
}

#[test]
fn indexed_listings_discard_earlier_cohorts_on_late_backend_or_metadata_failure() {
    let dirs: Vec<_> = (0..65)
        .map(|n| CString::new(format!("/dir-{n}")).unwrap())
        .collect();
    let options = vfsi_listing_options {
        max_entries: dirs.len(),
        max_path_bytes: 4096,
        attributes: VFSI_ATTR_MODE | VFSI_ATTR_UID | VFSI_ATTR_GID,
    };
    for (missing, fail_call, expected) in [
        (AttrMask::empty(), Some(3), libc::EIO),
        (AttrMask::UID | AttrMask::GID, None, libc::ENOTSUP),
    ] {
        let fixture = ListingFixture::with_failure(1, missing, 32, fail_call, None);
        let (status, indices, results) = fixture.run(&dirs, &options);
        assert_eq!(status, expected);
        assert_eq!(fixture.calls.lock().unwrap().len(), 3);
        assert!(indices.is_empty());
        assert!(results
            .iter()
            .all(|r| r.completion == 0 && r.delivered == 0));
    }
}

#[test]
fn indexed_listing_projection_errors_preserve_backend_and_budget_error_precedence() {
    let dirs: Vec<_> = (0..65)
        .map(|n| CString::new(format!("/dir-{n}")).unwrap())
        .collect();
    for (max_entries, fail_call, expected, calls) in [
        (65, Some(3), libc::EIO, 3),
        (32, None, libc::EFBIG, 2),
        (65, None, libc::ENOTSUP, 3),
    ] {
        // The first row lacks requested metadata. Collection must still honor
        // the original backend/quota failure precedence and deliver no prefix.
        let fixture = ListingFixture::with_failure(1, AttrMask::UID, 0, fail_call, None);
        let (status, indices, results) = fixture.run(
            &dirs,
            &vfsi_listing_options {
                max_entries,
                max_path_bytes: usize::MAX,
                attributes: VFSI_ATTR_MODE | VFSI_ATTR_UID,
            },
        );
        assert_eq!(status, expected);
        assert_eq!(fixture.calls.lock().unwrap().len(), calls);
        assert!(indices.is_empty());
        assert!(results
            .iter()
            .all(|r| r.completion == 0 && r.delivered == 0));
    }
}

#[test]
fn directory_byte_overflow_stops_before_retaining_or_visiting_more_entries() {
    for max_entries in [100, usize::MAX] {
        let mut visited = 0;
        let error = collect_bounded_entries(max_entries, 1, |cb| {
            for _ in 0..100 {
                visited += 1;
                if !cb(&VfAttrs {
                    file: FileOperand::from_path("/long-name"),
                    ..Default::default()
                }) {
                    break;
                }
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.raw_code(), libc::EFBIG as u32);
        assert_eq!(
            visited, 1,
            "overflow must stop enumeration at the first oversized entry"
        );
    }
}

#[test]
fn bounded_directory_accepts_exact_limits_and_rejects_only_the_probe() {
    let entry = VfAttrs {
        file: FileOperand::from_path("/a"),
        ..Default::default()
    };
    let exact = collect_bounded_entries(1, 2, |cb| {
        assert!(cb(&entry));
        Ok(())
    })
    .unwrap();
    assert_eq!(exact.len(), 1);
    for (count, bytes) in [(0, 2), (1, 1), (1, 4)] {
        let mut visited = 0;
        let error = collect_bounded_entries(count, bytes, |cb| {
            for _ in 0..3 {
                visited += 1;
                if !cb(&entry) {
                    break;
                }
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.raw_code(), libc::EFBIG as u32);
        assert_eq!(visited, if count == 1 && bytes == 4 { 2 } else { 1 });
    }
    let error = collect_bounded_entries(10, 100, |cb| {
        assert!(cb(&entry));
        Err(VfError::errno(0, libc::EIO as u32))
    })
    .unwrap_err();
    assert_eq!(
        error.raw_code(),
        libc::EIO as u32,
        "backend errors must not become successful prefixes"
    );
}

#[test]
fn bounded_c_adapters_prune_before_io_and_preserve_error_limits() {
    unsafe extern "C" fn walk(
        path: *const c_char,
        kind: u32,
        _depth: usize,
        _attrs: *const vfsi_attrs,
        data: *mut c_void,
    ) -> u32 {
        let (fs, visited) = &mut *(data as *mut (*mut vfsi_fs, Vec<String>));
        let path = CStr::from_ptr(path).to_string_lossy().into_owned();
        assert!(!path.ends_with("hidden"));
        assert_ne!(
            vfsi_capabilities(*fs),
            0,
            "callback must run without the backend lock"
        );
        visited.push(path.clone());
        if kind == 0 && path.ends_with("blocked") {
            1
        } else {
            0
        }
    }
    unsafe extern "C" fn listing(
        _name: *const c_char,
        _attrs: *const vfsi_attrs,
        data: *mut c_void,
    ) -> bool {
        *(data as *mut usize) += 1;
        true
    }
    unsafe extern "C" fn read(
        _path: *const c_char,
        _data: *const u8,
        _len: usize,
        data: *mut c_void,
    ) -> bool {
        *(data as *mut usize) += 1;
        true
    }
    let (root, path) = temp_root();
    std::fs::create_dir(root.path().join("blocked")).unwrap();
    std::fs::write(root.path().join("blocked/hidden"), b"hidden").unwrap();
    std::fs::write(root.path().join("a"), b"abc").unwrap();
    unsafe {
        let mut fs = std::ptr::null_mut();
        assert_eq!(
            vfsi_dummy_open_mount(path.as_ptr(), path.as_ptr(), &mut fs),
            0
        );
        let mut context = (fs, Vec::<String>::new());
        let options = vfsi_walk_options {
            max_entries: 3,
            max_path_bytes: 1024,
            max_depth: 10,
            attributes: VFSI_ATTR_MODE,
            sort_by_name: 1,
        };
        assert_eq!(
            vfsi_walk(
                fs,
                path.as_ptr(),
                &options,
                Some(walk),
                (&mut context as *mut (*mut vfsi_fs, Vec<String>)).cast()
            ),
            0
        );
        assert_eq!(context.1.len(), 5);
        let mut calls = 0usize;
        assert_eq!(
            vfsi_listdir_with_limits(
                fs,
                path.as_ptr(),
                1,
                1024,
                Some(listing),
                (&mut calls as *mut usize).cast()
            ),
            libc::EFBIG
        );
        let file = CString::new(root.path().join("a").as_os_str().as_bytes()).unwrap();
        assert_eq!(
            vfsi_read_paths_with_limit(
                fs,
                &file.as_ptr(),
                1,
                1,
                Some(read),
                (&mut calls as *mut usize).cast()
            ),
            libc::EFBIG
        );
        assert_eq!(calls, 0);
        vfsi_free(fs);
        assert_eq!(
            vfsi_nfs_from_mount(c"relative".as_ptr(), &mut fs),
            libc::EINVAL
        );
        assert!(fs.is_null());
    }
}

fn temp_root() -> (tempfile::TempDir, CString) {
    let root = tempfile::TempDir::new().unwrap();
    let path = CString::new(root.path().as_os_str().as_encoded_bytes()).unwrap();
    (root, path)
}

#[test]
fn file_review_setattrv_rejects_masks_without_value_fields_before_mutating_any_file() {
    let (root, root_path) = temp_root();
    let original = root.path().join("keep");
    std::fs::write(&original, b"original").unwrap();
    let path = CString::new("/keep").unwrap();
    // No ownership change can occur even on the unfixed implementation.
    let missing = CString::new("/missing").unwrap();
    let mut fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root_path.as_ptr(), &mut fs) }, 0);
    let allowed = VFSI_ATTR_MODE | VFSI_ATTR_SIZE | VFSI_ATTR_ATIME | VFSI_ATTR_MTIME;
    for bit in 0..u32::BITS {
        let unsupported = 1u32 << bit;
        if unsupported & allowed != 0 {
            continue;
        }
        let valid = vfsi_setattr_op {
            path: path.as_ptr(),
            mask: VFSI_ATTR_SIZE,
            mode: 0,
            size: 0,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
        };
        let ops = [
            valid,
            vfsi_setattr_op {
                path: missing.as_ptr(),
                mask: unsupported | VFSI_ATTR_SIZE,
                ..valid
            },
        ];
        let mut items = [vfsi_result::panic(); 2];
        let result = unsafe { vfsi_setattrv(fs, ops.as_ptr(), ops.len(), items.as_mut_ptr()) };
        assert_eq!(std::fs::read(&original).unwrap(), b"original", "bit {bit}");
        assert_eq!(result.category, VFSI_ERROR_INVALID_ARGUMENT, "bit {bit}");
        assert_eq!(result.err_no, libc::EINVAL as u32);
        assert_eq!(result.index, 1);
        assert_eq!(items[0].category, VFSI_ERROR_NOT_ATTEMPTED);
        assert_eq!(items[1].category, VFSI_ERROR_INVALID_ARGUMENT);
    }
    unsafe { vfsi_free(fs) };
}

#[test]
fn file_review_setattrv_accepts_all_value_fields_in_abi_v3() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let (root, root_path) = temp_root();
    let file = root.path().join("file");
    std::fs::write(&file, b"original").unwrap();
    let path = CString::new("/file").unwrap();
    let mut fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root_path.as_ptr(), &mut fs) }, 0);
    let ops = [vfsi_setattr_op {
        path: path.as_ptr(),
        mask: VFSI_ATTR_MODE | VFSI_ATTR_SIZE | VFSI_ATTR_ATIME | VFSI_ATTR_MTIME,
        mode: 0o640,
        size: 2,
        atime_sec: 123,
        atime_nsec: 456,
        mtime_sec: 789,
        mtime_nsec: 123,
    }];
    let mut items = [vfsi_result::panic()];
    let result = unsafe { vfsi_setattrv(fs, ops.as_ptr(), ops.len(), items.as_mut_ptr()) };
    unsafe { vfsi_free(fs) };
    assert_eq!(result.category, VFSI_ERROR_NONE);
    assert_eq!(items[0].category, VFSI_ERROR_NONE);
    let metadata = std::fs::metadata(&file).unwrap();
    assert_eq!(metadata.len(), 2);
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o640);
    assert_eq!((metadata.atime(), metadata.atime_nsec()), (123, 456));
    assert_eq!((metadata.mtime(), metadata.mtime_nsec()), (789, 123));
}

#[test]
fn unknown_transport_index_remains_unknown_for_c_callers() {
    let failure = vfsi_result::from_error(VfError::transport(None, "lost reply"));
    assert_eq!(failure.index, C_INDEX_UNKNOWN);
    let mut items = [vfsi_result::panic(), vfsi_result::panic()];
    fail_results(&mut items, failure, true);
    assert!(items
        .iter()
        .all(|item| item.category == VFSI_ERROR_INDETERMINATE));
}

#[test]
fn unattributed_status_keeps_its_errno_and_category_for_c_callers() {
    // A native NFS status is not an OS errno. Scalar helpers translate it;
    // structured vector results retain its native value and attribution.
    let error = vfsi_sync::error_from_rpc(vfsi_sync::RpcError::op(0, 10005), None);
    assert_eq!(error.index(), None);
    assert_eq!(vf_code(&error), libc::EIO);
    let failure = vfsi_result::from_error(error);
    assert_eq!(failure.index, C_INDEX_UNKNOWN);
    assert_eq!(failure.category, VFSI_ERROR_FILESYSTEM);
    assert_eq!(failure.err_no, 10005);
}

#[test]
fn close_failure_releases_only_confirmed_successes_including_later_siblings() {
    use vfsi_sync::OpError;
    let mut results = [vfsi_result::item_success(0); 4];
    let mut retired = Vec::new();
    let status = unit_results(
        "close",
        &mut results,
        Ok(vec![
            Ok(()),
            Err(OpError::failed(
                VfError::errno(None, libc::EIO as u32),
                None,
            )),
            Ok(()),
            Err(OpError::unknown(
                VfError::transport(None, "lost reply"),
                None,
            )),
        ]),
        |index| retired.push(index),
    );
    assert_eq!(retired, [0, 2]);
    assert_eq!(status.index, 1);
    assert_eq!(results[0].category, VFSI_ERROR_NONE);
    assert_eq!(results[1].category, VFSI_ERROR_FILESYSTEM);
    assert_eq!(results[2].category, VFSI_ERROR_NONE);
    assert_eq!(results[3].category, VFSI_ERROR_INDETERMINATE);
}

#[test]
fn smb_constructor_rejects_invalid_arguments_before_connecting() {
    let mut fs: *mut vfsi_fs = std::ptr::null_mut();
    assert_eq!(
        unsafe {
            vfsi_smb_open(
                std::ptr::null(),
                c"share".as_ptr(),
                c"".as_ptr(),
                c"".as_ptr(),
                c"".as_ptr(),
                &mut fs,
            )
        },
        libc::EINVAL
    );
    let invalid_utf8 = [0xff_u8, 0];
    assert_eq!(
        unsafe {
            vfsi_smb_open(
                invalid_utf8.as_ptr().cast(),
                c"share".as_ptr(),
                c"".as_ptr(),
                c"".as_ptr(),
                c"".as_ptr(),
                &mut fs,
            )
        },
        libc::EINVAL
    );
    assert!(fs.is_null());
}

#[test]
fn dummy_constructor_reports_root_setup_errors() {
    let directory =
        std::env::temp_dir().join(format!("vfsi-c-invalid-root-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir(&directory).unwrap();
    let file = directory.join("not-a-directory");
    std::fs::write(&file, b"x").unwrap();
    let file = CString::new(file.as_os_str().as_bytes()).unwrap();
    let mut filesystem: *mut vfsi_fs = std::ptr::null_mut();
    let result = unsafe { vfsi_dummy_open(file.as_ptr(), &mut filesystem) };
    assert!(matches!(
        result,
        value if value == libc::EEXIST || value == libc::ENOTDIR
    ));
    assert!(filesystem.is_null());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn dummy_backend_read_write_list() {
    let (_root, root) = temp_root();
    let mut fs: *mut vfsi_fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root.as_ptr(), &mut fs) }, 0);
    assert!(!fs.is_null());

    let dir = CString::new("/d").unwrap();
    assert_eq!(unsafe { vfsi_mkdir(fs, dir.as_ptr(), 0o755, 1) }, 0);
    let path = CString::new("/d/f.bin").unwrap();
    let fd = unsafe {
        vfsi_open(
            fs,
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o644,
        )
    };
    assert!(fd > 0);
    let data = b"hello".as_ptr() as *const c_void;
    assert_eq!(
        unsafe { vfsi_pwrite(fs, fd, data, 5, 0, std::ptr::null_mut()) },
        0
    );
    assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);

    let mut buf = [0u8; 8];
    let fd = unsafe { vfsi_open(fs, path.as_ptr(), libc::O_RDONLY, 0) };
    assert!(fd > 0);
    let mut got = 0usize;
    assert_eq!(
        unsafe { vfsi_pread(fs, fd, buf.as_mut_ptr() as *mut c_void, 8, 0, &mut got) },
        0
    );
    assert_eq!(got, 5);
    assert_eq!(&buf[..5], b"hello");
    assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);

    let mut seen: Vec<String> = Vec::new();
    unsafe extern "C" fn callback(
        name: *const c_char,
        _: *const vfsi_attrs,
        user: *mut c_void,
    ) -> bool {
        let v = &mut *(user as *mut Vec<String>);
        v.push(CStr::from_ptr(name).to_string_lossy().into_owned());
        true
    }
    assert_eq!(
        unsafe {
            vfsi_listdir(
                fs,
                dir.as_ptr(),
                Some(callback),
                &mut seen as *mut _ as *mut c_void,
            )
        },
        0
    );
    assert_eq!(seen, vec!["f.bin"]);

    unsafe { vfsi_free(fs) };
}

#[test]
fn indexed_listings_report_empty_duplicates_stop_and_reject_overflow_before_callbacks() {
    let (_root, root) = temp_root();
    let mut fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root.as_ptr(), &mut fs) }, 0);
    let dir = CString::new("/dir").unwrap();
    let empty = CString::new("/empty").unwrap();
    for path in [&dir, &empty] {
        assert_eq!(unsafe { vfsi_mkdir(fs, path.as_ptr(), 0o755, 0) }, 0);
    }
    let file = CString::new("/dir/file").unwrap();
    let fd = unsafe { vfsi_open(fs, file.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o644) };
    assert!(fd > 0);
    assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);
    struct Seen {
        fs: *mut vfsi_fs,
        indices: Vec<usize>,
        stop: bool,
    }
    unsafe extern "C" fn cb(
        index: usize,
        dir: *const c_char,
        _: *const c_char,
        _: *const vfsi_attrs,
        user: *mut c_void,
    ) -> bool {
        let seen = &mut *(user as *mut Seen);
        let mut attrs = std::mem::zeroed();
        // Re-entering must not deadlock: delivery is outside the backend lock.
        assert_eq!(vfsi_stat(seen.fs, dir, &mut attrs), 0);
        seen.indices.push(index);
        !seen.stop
    }
    let dirs = [dir.as_ptr(), empty.as_ptr(), dir.as_ptr()];
    let mut options = vfsi_listing_options {
        max_entries: 2,
        max_path_bytes: 18,
        attributes: VFSI_ATTR_MODE,
    };
    let mut results = [vfsi_listing_result::default(); 3];
    let mut seen = Seen {
        fs,
        indices: vec![],
        stop: false,
    };
    let run = |options: &vfsi_listing_options,
               seen: &mut Seen,
               results: &mut [vfsi_listing_result; 3]| unsafe {
        vfsi_listdirs(
            fs,
            dirs.as_ptr(),
            3,
            options,
            Some(cb),
            seen as *mut _ as *mut c_void,
            results.as_mut_ptr(),
        )
    };
    assert_eq!(run(&options, &mut seen, &mut results), 0);
    seen.indices.sort_unstable();
    assert_eq!(seen.indices, [0, 2]);
    assert!(results.iter().all(|result| result.completion == 1));
    assert_eq!(results.map(|result| result.delivered), [1, 0, 1]);
    for (entries, bytes) in [(1, 18), (2, 17)] {
        options.max_entries = entries;
        options.max_path_bytes = bytes;
        seen.indices.clear();
        assert_eq!(run(&options, &mut seen, &mut results), libc::EFBIG);
        assert!(seen.indices.is_empty());
        assert!(results
            .iter()
            .all(|result| result.completion == 0 && result.delivered == 0));
    }
    options.max_entries = 2;
    options.max_path_bytes = 18;
    seen.stop = true;
    assert_eq!(run(&options, &mut seen, &mut results), 0);
    assert_eq!(seen.indices.len(), 1);
    assert!(results.iter().all(|result| result.completion == 2));
    let missing = CString::new("/missing").unwrap();
    let bad_dirs = [dir.as_ptr(), missing.as_ptr()];
    seen.indices.clear();
    seen.stop = false;
    assert_eq!(
        unsafe {
            vfsi_listdirs(
                fs,
                bad_dirs.as_ptr(),
                2,
                &options,
                Some(cb),
                &mut seen as *mut _ as *mut c_void,
                results.as_mut_ptr(),
            )
        },
        libc::ENOENT
    );
    assert!(seen.indices.is_empty());
    unsafe {
        vfsi_free(fs);
    }
}

#[test]
fn recursive_listdirv_strips_backend_root_before_mount_mapping() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("exports/git/project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("file.bin"), b"data").unwrap();

    let backend = Box::new(vfsi_posix::backend(temp.path()).unwrap()) as Box<dyn BindingBackend>;
    let raw = make_fs(
        backend,
        PathBuf::from("/mnt/repos"),
        PathBuf::from("/exports/git"),
    );
    let dir = CString::new("/mnt/repos").unwrap();

    #[derive(Default)]
    struct Seen(Vec<(String, String)>);
    unsafe extern "C" fn cb(
        dir: *const c_char,
        name: *const c_char,
        _: *const vfsi_attrs,
        user: *mut c_void,
    ) -> bool {
        let seen = &mut *(user as *mut Seen);
        seen.0.push((
            CStr::from_ptr(dir).to_string_lossy().into_owned(),
            CStr::from_ptr(name).to_string_lossy().into_owned(),
        ));
        true
    }

    let dirs = [dir.as_ptr()];
    let mut seen = Seen::default();
    assert_eq!(
        unsafe {
            vfsi_listdirv(
                raw,
                dirs.as_ptr(),
                1,
                0,
                true,
                Some(cb),
                &mut seen as *mut _ as *mut c_void,
            )
        },
        0
    );
    assert_eq!(
        seen.0,
        vec![
            ("/mnt/repos".into(), "project".into()),
            ("/mnt/repos/project".into(), "file.bin".into()),
        ]
    );

    unsafe { vfsi_free(raw) };
}

#[test]
fn dummy_backend_listdirv_maps_dirs_to_entries() {
    let (_root, root) = temp_root();
    let mut fs: *mut vfsi_fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root.as_ptr(), &mut fs) }, 0);
    assert!(!fs.is_null());

    let d1 = CString::new("/v/d1").unwrap();
    let d2 = CString::new("/v/d2").unwrap();
    assert_eq!(unsafe { vfsi_mkdir(fs, d1.as_ptr(), 0o755, 1) }, 0);
    assert_eq!(unsafe { vfsi_mkdir(fs, d2.as_ptr(), 0o755, 1) }, 0);
    let p1 = CString::new("/v/d1/one.bin").unwrap();
    let p2 = CString::new("/v/d2/two.bin").unwrap();
    for p in [&p1, &p2] {
        let fd = unsafe {
            vfsi_open(
                fs,
                p.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                0o644,
            )
        };
        assert!(fd > 0);
        assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);
    }

    #[derive(Default)]
    struct Seen {
        rows: Vec<(String, String)>,
    }
    unsafe extern "C" fn cb(
        dir: *const c_char,
        name: *const c_char,
        _: *const vfsi_attrs,
        user: *mut c_void,
    ) -> bool {
        let seen = &mut *(user as *mut Seen);
        seen.rows.push((
            CStr::from_ptr(dir).to_string_lossy().into_owned(),
            CStr::from_ptr(name).to_string_lossy().into_owned(),
        ));
        true
    }

    let dirs = [d1.as_ptr(), d2.as_ptr()];
    let mut seen = Seen::default();
    assert_eq!(
        unsafe {
            vfsi_listdirv(
                fs,
                dirs.as_ptr(),
                dirs.len(),
                0,
                false,
                Some(cb),
                &mut seen as *mut _ as *mut c_void,
            )
        },
        0
    );
    assert_eq!(
        seen.rows,
        vec![
            ("/v/d1".to_string(), "one.bin".to_string()),
            ("/v/d2".to_string(), "two.bin".to_string()),
        ]
    );

    let sub = CString::new("/v/d1/sub").unwrap();
    let nested = CString::new("/v/d1/sub/nested.bin").unwrap();
    assert_eq!(unsafe { vfsi_mkdir(fs, sub.as_ptr(), 0o755, 1) }, 0);
    let fd = unsafe {
        vfsi_open(
            fs,
            nested.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o644,
        )
    };
    assert!(fd > 0);
    assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);
    let one_dir = [d1.as_ptr()];
    seen.rows.clear();
    assert_eq!(
        unsafe {
            vfsi_listdirv(
                fs,
                one_dir.as_ptr(),
                one_dir.len(),
                0,
                true,
                Some(cb),
                &mut seen as *mut _ as *mut c_void,
            )
        },
        0
    );
    assert!(seen
        .rows
        .contains(&("/v/d1/sub".to_string(), "nested.bin".to_string())));

    unsafe { vfsi_free(fs) };
}

#[test]
fn listdirv_callback_can_reenter_filesystem() {
    if vfsi_sync::test_support::supervise_with_deadline(
        "tests::listdirv_callback_can_reenter_filesystem",
    ) {
        return;
    }
    let (_root, root) = temp_root();
    let mut fs: *mut vfsi_fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root.as_ptr(), &mut fs) }, 0);
    let dir = CString::new("/reentrant").unwrap();
    let path = CString::new("/reentrant/file").unwrap();
    assert_eq!(unsafe { vfsi_mkdir(fs, dir.as_ptr(), 0o755, 1) }, 0);
    let fd = unsafe { vfsi_open(fs, path.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644) };
    assert!(fd > 0);
    assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);

    struct Reentrant {
        fs: *mut vfsi_fs,
        path: CString,
        stat_result: c_int,
    }
    unsafe extern "C" fn cb(
        _: *const c_char,
        _: *const c_char,
        _: *const vfsi_attrs,
        user: *mut c_void,
    ) -> bool {
        let state = &mut *(user as *mut Reentrant);
        let mut attrs = std::mem::zeroed();
        state.stat_result = vfsi_stat(state.fs, state.path.as_ptr(), &mut attrs);
        false
    }

    let dirs = [dir.as_ptr()];
    let mut state = Reentrant {
        fs,
        path,
        stat_result: -1,
    };
    assert_eq!(
        unsafe {
            vfsi_listdirv(
                fs,
                dirs.as_ptr(),
                dirs.len(),
                0,
                false,
                Some(cb),
                &mut state as *mut _ as *mut c_void,
            )
        },
        0
    );
    assert_eq!(state.stat_result, 0);
    unsafe { vfsi_free(fs) };
}

#[test]
fn poisoned_backend_mutex_returns_eio() {
    let (_root, root) = temp_root();
    let mut fs: *mut vfsi_fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root.as_ptr(), &mut fs) }, 0);
    let handle = unsafe { &*fs };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = handle.fs.lock().unwrap();
        panic!("poison backend mutex");
    }));

    let path = CString::new("/").unwrap();
    let mut attrs = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { vfsi_stat(fs, path.as_ptr(), &mut attrs) },
        libc::EIO
    );
    unsafe { vfsi_free(fs) };
}

#[test]
fn dummy_open_mount_accepts_kernel_paths() {
    let (_root, root) = temp_root();
    let mut fs: *mut vfsi_fs = std::ptr::null_mut();
    assert_eq!(
        unsafe { vfsi_dummy_open_mount(root.as_ptr(), root.as_ptr(), &mut fs) },
        0
    );
    assert!(!fs.is_null());

    let mut path = root.as_bytes().to_vec();
    path.extend_from_slice(b"/mount-dir/file.bin");
    let path = CString::new(path).unwrap();
    let mut dir = root.as_bytes().to_vec();
    dir.extend_from_slice(b"/mount-dir");
    let dir = CString::new(dir).unwrap();

    assert_eq!(unsafe { vfsi_mkdir(fs, dir.as_ptr(), 0o755, 1) }, 0);
    let fd = unsafe {
        vfsi_open(
            fs,
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o644,
        )
    };
    assert!(fd > 0);
    assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);
    assert_eq!(unsafe { vfsi_remove(fs, path.as_ptr()) }, 0);
    assert_eq!(unsafe { vfsi_remove(fs, dir.as_ptr()) }, 0);

    unsafe { vfsi_free(fs) };
}

#[test]
fn mount_mapping_includes_backend_root_and_rejects_escape() {
    let (_root, root) = temp_root();
    let backend =
        Box::new(vfsi_posix::backend(PathBuf::from(root.to_string_lossy().into_owned())).unwrap())
            as Box<dyn BindingBackend>;
    let raw = make_fs(
        backend,
        PathBuf::from("/mnt/repos"),
        PathBuf::from("/exports/git"),
    );
    let fs = unsafe { &*raw };

    assert_eq!(
        path_for(fs, Path::new("/mnt/repos/project/objects")),
        Some(PathBuf::from("/exports/git/project/objects"))
    );
    assert_eq!(path_for(fs, Path::new("/mnt/repos/../escape")), None);

    unsafe { vfsi_free(raw) };
}

#[test]
fn read_paths_batches_full_file_contents() {
    let (_root, root) = temp_root();
    let mut fs: *mut vfsi_fs = std::ptr::null_mut();
    assert_eq!(
        unsafe { vfsi_dummy_open_mount(root.as_ptr(), root.as_ptr(), &mut fs) },
        0
    );
    assert!(!fs.is_null());

    let mut dir = root.as_bytes().to_vec();
    dir.extend_from_slice(b"/read-dir");
    let dir = CString::new(dir).unwrap();
    assert_eq!(unsafe { vfsi_mkdir(fs, dir.as_ptr(), 0o755, 1) }, 0);

    let path_a = {
        let mut p = dir.as_bytes().to_vec();
        p.extend_from_slice(b"/a.bin");
        CString::new(p).unwrap()
    };
    let path_b = {
        let mut p = dir.as_bytes().to_vec();
        p.extend_from_slice(b"/b.bin");
        CString::new(p).unwrap()
    };
    for (path, contents) in [(&path_a, &b"first"[..]), (&path_b, &b"second-longer"[..])] {
        let fd = unsafe {
            vfsi_open(
                fs,
                path.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                0o644,
            )
        };
        assert!(fd > 0);
        assert_eq!(
            unsafe {
                vfsi_pwrite(
                    fs,
                    fd,
                    contents.as_ptr() as *const c_void,
                    contents.len(),
                    0,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);
    }

    #[derive(Default)]
    struct Seen {
        rows: Vec<(String, Vec<u8>)>,
    }
    unsafe extern "C" fn cb(
        path: *const c_char,
        data: *const u8,
        len: usize,
        user: *mut c_void,
    ) -> bool {
        let seen = &mut *(user as *mut Seen);
        let bytes = unsafe { std::slice::from_raw_parts(data, len) };
        seen.rows.push((
            CStr::from_ptr(path).to_string_lossy().into_owned(),
            bytes.to_vec(),
        ));
        true
    }

    let paths = [path_a.as_ptr(), path_b.as_ptr()];
    let mut seen = Seen::default();
    assert_eq!(
        unsafe {
            vfsi_read_paths(
                fs,
                paths.as_ptr(),
                paths.len(),
                Some(cb),
                &mut seen as *mut _ as *mut c_void,
            )
        },
        0
    );
    assert_eq!(seen.rows[0].1, b"first");
    assert_eq!(seen.rows[1].1, b"second-longer");

    unsafe { vfsi_free(fs) };
}

#[test]
fn openv_backend_failure_marks_submitted_items_indeterminate() {
    let (root, root_path) = temp_root();
    let existing = root.path().join("truncated-before-failure");
    std::fs::write(&existing, b"old contents").unwrap();
    let first = CString::new("/truncated-before-failure").unwrap();
    let second = CString::new("/missing-parent/never-created").unwrap();
    let mut fs: *mut vfsi_fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root_path.as_ptr(), &mut fs) }, 0);

    let mut ops = [
        vfsi_open_op {
            path: first.as_ptr(),
            flags: libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            mode: 0o644,
            fd: -1,
        },
        vfsi_open_op {
            path: second.as_ptr(),
            flags: libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            mode: 0o644,
            fd: -1,
        },
    ];
    let mut items = [vfsi_result::panic(); 2];
    let result = unsafe { vfsi_openv(fs, ops.as_mut_ptr(), ops.len(), items.as_mut_ptr()) };

    assert_eq!(result.index, 1);
    assert_eq!(result.category, VFSI_ERROR_FILESYSTEM);
    assert_eq!(result.err_no, libc::ENOENT as u32);
    assert_eq!(items[0].category, VFSI_ERROR_INDETERMINATE);
    assert_eq!(items[1].category, VFSI_ERROR_FILESYSTEM);
    assert_eq!(items[1].index, 1);
    assert_eq!(std::fs::read(existing).unwrap(), b"");

    unsafe { vfsi_free(fs) };
}

#[test]
fn abi_v3_vector_io_and_bounded_streaming() {
    let (_root, root) = temp_root();
    let mut fs: *mut vfsi_fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root.as_ptr(), &mut fs) }, 0);
    let mut item_results = [vfsi_result::panic(); 2];
    let mut one_result = [vfsi_result::panic(); 1];

    let dir_a = CString::new("/v3-a").unwrap();
    let dir_b = CString::new("/v3-b").unwrap();
    let mkdir_ops = [
        vfsi_mkdir_op {
            path: dir_a.as_ptr(),
            mode: 0o755,
        },
        vfsi_mkdir_op {
            path: dir_b.as_ptr(),
            mode: 0o755,
        },
    ];
    let result = unsafe {
        vfsi_mkdirv(
            fs,
            mkdir_ops.as_ptr(),
            mkdir_ops.len(),
            item_results.as_mut_ptr(),
        )
    };
    assert_eq!(result.category, VFSI_ERROR_NONE);
    assert_eq!(result.index, 2);
    assert!(item_results
        .iter()
        .all(|result| result.category == VFSI_ERROR_NONE));

    let path_a = CString::new("/v3-a/a.bin").unwrap();
    let path_b = CString::new("/v3-b/b.bin").unwrap();
    let mut open_ops = [
        vfsi_open_op {
            path: path_a.as_ptr(),
            flags: libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            mode: 0o644,
            fd: -1,
        },
        vfsi_open_op {
            path: path_b.as_ptr(),
            flags: libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            mode: 0o644,
            fd: -1,
        },
    ];
    let result = unsafe {
        vfsi_openv(
            fs,
            open_ops.as_mut_ptr(),
            open_ops.len(),
            item_results.as_mut_ptr(),
        )
    };
    assert_eq!(result.category, VFSI_ERROR_NONE);
    assert!(open_ops.iter().all(|op| op.fd > 0));

    let data_a = b"abcdef";
    let data_b = b"1234567";
    let mut writes = [
        vfsi_pwrite_op {
            fd: open_ops[0].fd,
            buf: data_a.as_ptr().cast(),
            len: data_a.len(),
            offset: 0,
            wrote: 0,
        },
        vfsi_pwrite_op {
            fd: open_ops[1].fd,
            buf: data_b.as_ptr().cast(),
            len: data_b.len(),
            offset: 0,
            wrote: 0,
        },
    ];
    assert_eq!(
        unsafe {
            vfsi_pwritev(
                fs,
                writes.as_mut_ptr(),
                writes.len(),
                item_results.as_mut_ptr(),
            )
        }
        .category,
        VFSI_ERROR_NONE
    );
    assert_eq!([writes[0].wrote, writes[1].wrote], [6, 7]);

    let mut buf_a = [0u8; 8];
    let mut buf_b = [0u8; 8];
    let mut reads = [
        vfsi_pread_op {
            fd: open_ops[0].fd,
            buf: buf_a.as_mut_ptr().cast(),
            len: buf_a.len(),
            offset: 0,
            got: 0,
        },
        vfsi_pread_op {
            fd: open_ops[1].fd,
            buf: buf_b.as_mut_ptr().cast(),
            len: buf_b.len(),
            offset: 0,
            got: 0,
        },
    ];
    assert_eq!(
        unsafe {
            vfsi_preadv(
                fs,
                reads.as_mut_ptr(),
                reads.len(),
                item_results.as_mut_ptr(),
            )
        }
        .category,
        VFSI_ERROR_NONE
    );
    assert_eq!(&buf_a[..reads[0].got], data_a);
    assert_eq!(&buf_b[..reads[1].got], data_b);

    let fds = [open_ops[0].fd, open_ops[1].fd];
    assert_eq!(
        unsafe { vfsi_closev(fs, fds.as_ptr(), fds.len(), item_results.as_mut_ptr(),) }.category,
        VFSI_ERROR_NONE
    );

    #[derive(Default)]
    struct Streamed {
        files: [Vec<u8>; 2],
        max_chunk: usize,
    }
    unsafe extern "C" fn stream_cb(
        _: *const c_char,
        index: usize,
        _: u64,
        data: *const u8,
        len: usize,
        _: bool,
        userdata: *mut c_void,
    ) -> bool {
        let state = &mut *userdata.cast::<Streamed>();
        state.max_chunk = state.max_chunk.max(len);
        state.files[index].extend_from_slice(std::slice::from_raw_parts(data, len));
        true
    }
    let paths = [path_a.as_ptr(), path_b.as_ptr()];
    let mut streamed = Streamed::default();
    let result = unsafe {
        vfsi_read_streamv(
            fs,
            paths.as_ptr(),
            paths.len(),
            3,
            4,
            Some(stream_cb),
            &mut streamed as *mut _ as *mut c_void,
        )
    };
    assert_eq!(result.category, VFSI_ERROR_NONE);
    assert!(streamed.max_chunk <= 3);
    assert_eq!(streamed.files[0], data_a);
    assert_eq!(streamed.files[1], data_b);

    let mut stats = [
        vfsi_stat_op {
            path: path_a.as_ptr(),
            attrs: unsafe { std::mem::zeroed() },
        },
        vfsi_stat_op {
            path: path_b.as_ptr(),
            attrs: unsafe { std::mem::zeroed() },
        },
    ];
    assert_eq!(
        unsafe {
            vfsi_statv(
                fs,
                stats.as_mut_ptr(),
                stats.len(),
                item_results.as_mut_ptr(),
            )
        }
        .category,
        VFSI_ERROR_NONE
    );
    assert_eq!([stats[0].attrs.size, stats[1].attrs.size], [6, 7]);

    let setattr = [vfsi_setattr_op {
        path: path_a.as_ptr(),
        mask: VFSI_ATTR_SIZE,
        mode: 0,
        size: 2,
        atime_sec: 0,
        atime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
    }];
    assert_eq!(
        unsafe { vfsi_setattrv(fs, setattr.as_ptr(), setattr.len(), one_result.as_mut_ptr(),) }
            .category,
        VFSI_ERROR_NONE
    );

    let renamed_a = CString::new("/v3-a/renamed.bin").unwrap();
    let renamed_b = CString::new("/v3-b/renamed.bin").unwrap();
    let renames = [
        vfsi_rename_op {
            oldpath: path_a.as_ptr(),
            newpath: renamed_a.as_ptr(),
        },
        vfsi_rename_op {
            oldpath: path_b.as_ptr(),
            newpath: renamed_b.as_ptr(),
        },
    ];
    assert_eq!(
        unsafe {
            vfsi_renamev(
                fs,
                renames.as_ptr(),
                renames.len(),
                item_results.as_mut_ptr(),
            )
        }
        .category,
        VFSI_ERROR_NONE
    );
    let remove_paths = [renamed_a.as_ptr(), renamed_b.as_ptr()];
    assert_eq!(
        unsafe {
            vfsi_removev(
                fs,
                remove_paths.as_ptr(),
                remove_paths.len(),
                item_results.as_mut_ptr(),
            )
        }
        .category,
        VFSI_ERROR_NONE
    );
    assert_eq!(
        unsafe {
            vfsi_removev(
                fs,
                [dir_a.as_ptr(), dir_b.as_ptr()].as_ptr(),
                2,
                item_results.as_mut_ptr(),
            )
        }
        .category,
        VFSI_ERROR_NONE
    );

    let missing = CString::new("/missing").unwrap();
    let root_path = CString::new("/").unwrap();
    let mut failed_stats = [
        vfsi_stat_op {
            path: missing.as_ptr(),
            attrs: unsafe { std::mem::zeroed() },
        },
        vfsi_stat_op {
            path: root_path.as_ptr(),
            attrs: unsafe { std::mem::zeroed() },
        },
    ];
    let failed = unsafe {
        vfsi_statv(
            fs,
            failed_stats.as_mut_ptr(),
            failed_stats.len(),
            item_results.as_mut_ptr(),
        )
    };
    assert_eq!(failed.index, 0);
    assert_eq!(failed.category, VFSI_ERROR_FILESYSTEM);
    assert_eq!(item_results[0].category, VFSI_ERROR_FILESYSTEM);
    assert_eq!(item_results[1].category, VFSI_ERROR_INDETERMINATE);
    unsafe { vfsi_free(fs) };
}

#[test]
fn close_missing_reply_keeps_known_success_and_legacy_transport_category() {
    let mut results = [vfsi_result::item_success(0); 2];
    let mut retired = Vec::new();
    let status = unit_results("close", &mut results, Ok(vec![Ok(())]), |index| {
        retired.push(index)
    });
    assert_eq!(retired, [0]);
    assert_eq!(status.index, 1);
    assert_eq!(status.category, VFSI_ERROR_TRANSPORT);
    assert_eq!(results[0].category, VFSI_ERROR_NONE);
    assert_eq!(results[1].category, VFSI_ERROR_INDETERMINATE);
}

#[test]
fn abi_v3_mkdir_reports_independent_successes_after_a_collision() {
    let (root_path, root) = temp_root();
    let mut fs: *mut vfsi_fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root.as_ptr(), &mut fs) }, 0);
    std::fs::create_dir(root_path.path().join("existing")).unwrap();
    let names = [
        CString::new("/first").unwrap(),
        CString::new("/existing").unwrap(),
        CString::new("/last").unwrap(),
    ];
    let ops: Vec<_> = names
        .iter()
        .map(|name| vfsi_mkdir_op {
            path: name.as_ptr(),
            mode: 0o700,
        })
        .collect();
    let mut results = [vfsi_result::panic(); 3];
    let status = unsafe { vfsi_mkdirv(fs, ops.as_ptr(), ops.len(), results.as_mut_ptr()) };
    assert_eq!(status.index, 1);
    assert_eq!(status.err_no, libc::EEXIST as u32);
    assert_eq!(results[0].category, VFSI_ERROR_NONE);
    assert_eq!(results[1].category, VFSI_ERROR_FILESYSTEM);
    assert_eq!(results[2].category, VFSI_ERROR_NONE);
    assert!(root_path.path().join("first").is_dir() && root_path.path().join("last").is_dir());
    unsafe { vfsi_free(fs) };
}

#[test]
fn unit_namespace_rejection_never_exports_an_out_of_range_error_index() {
    let mut results = [
        vfsi_result::base(0, VFSI_ERROR_NOT_ATTEMPTED, 0, "not attempted"),
        vfsi_result::base(1, VFSI_ERROR_NOT_ATTEMPTED, 0, "not attempted"),
    ];
    let mut retired = Vec::new();
    let status = unit_results(
        "mkdir",
        &mut results,
        Err(VfError::errno(99, libc::EIO as u32)),
        |index| retired.push(index),
    );
    assert_eq!(status.index, C_INDEX_UNKNOWN);
    assert_eq!(status.category, VFSI_ERROR_TRANSPORT);
    assert!(retired.is_empty());
    assert!(results
        .iter()
        .all(|result| result.category == VFSI_ERROR_NOT_ATTEMPTED));
}

#[test]
fn setattrv_reports_later_successes_without_changing_abi_v3_layouts() {
    let (root, root_path) = temp_root();
    for name in ["first", "last"] {
        std::fs::write(root.path().join(name), b"original").unwrap();
    }
    let paths = [c"/first", c"/missing", c"/last"];
    let ops: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| vfsi_setattr_op {
            path: path.as_ptr(),
            mask: VFSI_ATTR_SIZE,
            mode: 0,
            size: index as u64 + 1,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
        })
        .collect();
    let mut fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root_path.as_ptr(), &mut fs) }, 0);
    let mut items = [vfsi_result::panic(); 3];
    let status = unsafe { vfsi_setattrv(fs, ops.as_ptr(), ops.len(), items.as_mut_ptr()) };
    unsafe { vfsi_free(fs) };
    assert_eq!(status.index, 1);
    assert_eq!(items[0].category, VFSI_ERROR_NONE);
    assert_eq!(items[1].category, VFSI_ERROR_FILESYSTEM);
    assert_eq!(items[1].err_no, libc::ENOENT as u32);
    assert_eq!(items[2].category, VFSI_ERROR_NONE);
    assert_eq!(
        std::fs::metadata(root.path().join("first")).unwrap().len(),
        1
    );
    assert_eq!(
        std::fs::metadata(root.path().join("last")).unwrap().len(),
        3
    );
}

#[test]
fn renamev_reports_nonprefix_successes_and_retains_abi_v3() {
    let (root, root_path) = temp_root();
    std::fs::write(root.path().join("first"), b"first").unwrap();
    std::fs::write(root.path().join("last"), b"last").unwrap();
    std::fs::write(root.path().join("one"), b"old").unwrap();
    let pairs = [
        (c"/first", c"/one"),
        (c"/missing", c"/two"),
        (c"/last", c"/three"),
    ];
    let ops: Vec<_> = pairs
        .iter()
        .map(|(source, target)| vfsi_rename_op {
            oldpath: source.as_ptr(),
            newpath: target.as_ptr(),
        })
        .collect();
    let mut fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root_path.as_ptr(), &mut fs) }, 0);
    let mut items = [vfsi_result::panic(); 3];
    let status = unsafe { vfsi_renamev(fs, ops.as_ptr(), ops.len(), items.as_mut_ptr()) };
    unsafe { vfsi_free(fs) };
    assert_eq!(status.index, 1);
    assert_eq!(items[0].category, VFSI_ERROR_NONE);
    assert_eq!(items[1].category, VFSI_ERROR_FILESYSTEM);
    assert_eq!(items[1].err_no, libc::ENOENT as u32);
    assert_eq!(items[2].category, VFSI_ERROR_NONE);
    assert_eq!(std::fs::read(root.path().join("one")).unwrap(), b"first");
    assert_eq!(std::fs::read(root.path().join("three")).unwrap(), b"last");
}

#[test]
fn copyv_reports_nonprefix_successes_without_changing_abi_v3() {
    let (root, root_path) = temp_root();
    for (name, bytes) in [
        ("first", b"first".as_slice()),
        ("last", b"last".as_slice()),
        ("one", b"long stale tail".as_slice()),
    ] {
        std::fs::write(root.path().join(name), bytes).unwrap();
    }
    let pairs = [
        (c"/first", c"/one"),
        (c"/missing", c"/two"),
        (c"/last", c"/three"),
    ];
    let ops: Vec<_> = pairs
        .iter()
        .map(|(source, destination)| vfsi_copy_op {
            src: source.as_ptr(),
            src_offset: 0,
            dst: destination.as_ptr(),
            dst_offset: 0,
            length: 0,
            to_eof: 1,
        })
        .collect();
    let mut fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root_path.as_ptr(), &mut fs) }, 0);
    let mut items = [vfsi_result::panic(); 3];
    let status = unsafe { vfsi_copyv(fs, ops.as_ptr(), ops.len(), items.as_mut_ptr()) };
    unsafe { vfsi_free(fs) };
    assert_eq!(status.index, 1);
    assert_eq!(items[0].category, VFSI_ERROR_NONE);
    assert_eq!(items[1].category, VFSI_ERROR_FILESYSTEM);
    assert_eq!(items[1].err_no, libc::ENOENT as u32);
    assert_eq!(items[2].category, VFSI_ERROR_NONE);
    assert_eq!(std::fs::read(root.path().join("one")).unwrap(), b"first");
    assert_eq!(std::fs::read(root.path().join("three")).unwrap(), b"last");
    assert!(!root.path().join("two").exists());
}

#[test]
fn removev_exports_nonprefix_successes_with_abi_v3_item_results() {
    let (root, root_path) = temp_root();
    std::fs::write(root.path().join("first"), b"first").unwrap();
    std::fs::write(root.path().join("last"), b"last").unwrap();
    let paths = [c"/first", c"/missing", c"/last"];
    let paths: Vec<_> = paths.iter().map(|path| path.as_ptr()).collect();
    let mut fs = std::ptr::null_mut();
    assert_eq!(unsafe { vfsi_dummy_open(root_path.as_ptr(), &mut fs) }, 0);
    let mut items = [vfsi_result::panic(); 3];
    let status = unsafe { vfsi_removev(fs, paths.as_ptr(), paths.len(), items.as_mut_ptr()) };
    unsafe {
        vfsi_free(fs);
    }
    assert_eq!(status.index, 1);
    assert_eq!(items[0].category, VFSI_ERROR_NONE);
    assert_eq!(items[1].category, VFSI_ERROR_FILESYSTEM);
    assert_eq!(items[1].err_no, libc::ENOENT as u32);
    assert_eq!(items[2].category, VFSI_ERROR_NONE);
    assert!(!root.path().join("first").exists());
    assert!(!root.path().join("last").exists());
}

#[test]
fn legacy_listings_reject_unrepresentable_names_instead_of_delivering_empty_names() {
    unsafe extern "C" fn scalar(_: *const c_char, _: *const vfsi_attrs, data: *mut c_void) -> bool {
        *(data as *mut usize) += 1;
        true
    }
    unsafe extern "C" fn vector(
        _: *const c_char,
        _: *const c_char,
        _: *const vfsi_attrs,
        data: *mut c_void,
    ) -> bool {
        *(data as *mut usize) += 1;
        true
    }
    for malformed_reply in [
        MalformedReply::ListingNulName,
        MalformedReply::ListingMissingName,
        MalformedReply::ListingNulDirectory,
    ] {
        let fs = make_fs(
            Box::new(ListingBackend {
                calls: Default::default(),
                entries_per_dir: 1,
                emitted: 0,
                missing: AttrMask::empty(),
                missing_after: usize::MAX,
                fail_call: None,
                write_calls: None,
                open_preflight_error: None,
                read_size: None,
                open_result_count: None,
                read_result_count: None,
                write_result_count: None,
                close_attempts: None,
                close_failure_fd: None,
                malformed_reply: Some(malformed_reply),
            }),
            PathBuf::from("/"),
            PathBuf::from("/"),
        );
        let dir = CString::new("/dir").unwrap();
        let mut calls = 0usize;
        let data = (&mut calls as *mut usize).cast();
        unsafe {
            if malformed_reply != MalformedReply::ListingNulDirectory {
                assert_eq!(
                    vfsi_listdir(fs, dir.as_ptr(), Some(scalar), data),
                    libc::EBADMSG
                );
            }
            assert_eq!(
                vfsi_listdirv(
                    fs,
                    [dir.as_ptr()].as_ptr(),
                    1,
                    10,
                    false,
                    Some(vector),
                    data
                ),
                libc::EBADMSG
            );
            vfsi_free(fs);
        }
        assert_eq!(
            calls, 0,
            "invalid names must not become successful empty callbacks"
        );
    }
}

fn independent_io_fixture(malformed_reply: MalformedReply) -> *mut vfsi_fs {
    let fs = make_fs(
        Box::new(ListingBackend {
            calls: Default::default(),
            entries_per_dir: 0,
            emitted: 0,
            missing: AttrMask::empty(),
            missing_after: 0,
            fail_call: None,
            write_calls: if malformed_reply == MalformedReply::WriteReject {
                None
            } else {
                Some(Default::default())
            },
            open_preflight_error: None,
            read_size: if malformed_reply == MalformedReply::ReadReject {
                None
            } else {
                Some(2)
            },
            open_result_count: None,
            read_result_count: None,
            write_result_count: None,
            close_attempts: None,
            close_failure_fd: None,
            malformed_reply: Some(malformed_reply),
        }),
        PathBuf::from("/"),
        PathBuf::from("/"),
    );
    let fs_ref = unsafe { &*fs };
    for fd in 10..13 {
        fs_ref.files.lock().unwrap().insert(
            fd,
            CFile {
                file: FileOperand::from_fd(fd),
                append: false,
            },
        );
    }
    fs
}

#[test]
fn extended_vectors_keep_siblings_and_confirmed_failed_prefixes() {
    let fs = independent_io_fixture(MalformedReply::ReadMiddleTransport);
    let mut buffers = [[0xa5u8; 2]; 3];
    let mut reads: Vec<_> = buffers
        .iter_mut()
        .enumerate()
        .map(|(i, b)| vfsi_pread_op {
            fd: 10 + i as i32,
            buf: b.as_mut_ptr().cast(),
            len: 2,
            offset: 9,
            got: 99,
        })
        .collect();
    let mut results: [std::mem::MaybeUninit<vfsi_io_result>; 3] =
        unsafe { std::mem::MaybeUninit::uninit().assume_init() };
    let result = unsafe { vfsi_preadv_ex(fs, reads.as_mut_ptr(), 3, results.as_mut_ptr().cast()) };
    assert_eq!(result.category, VFSI_ERROR_NONE);
    let results = results.map(|r| unsafe { r.assume_init() });
    assert_eq!(results.map(|r| r.failure_state), [0, 3, 0]);
    assert_eq!(reads.iter().map(|r| r.got).collect::<Vec<_>>(), [2, 1, 2]);
    assert_eq!(buffers, [*b"xx", [b'p', 0xa5], *b"xx"]);
    assert_eq!(results[1].offset, 9);
    assert_eq!(results[1].offset_known, 1);
    unsafe {
        vfsi_free(fs);
    }

    let fs = independent_io_fixture(MalformedReply::WriteMiddlePartial);
    let mut writes: Vec<_> = (0..3)
        .map(|i| vfsi_pwrite_op {
            fd: 10 + i,
            buf: b"ab".as_ptr().cast(),
            len: 2,
            offset: 9,
            wrote: 99,
        })
        .collect();
    let mut results: [std::mem::MaybeUninit<vfsi_io_result>; 3] =
        unsafe { std::mem::MaybeUninit::uninit().assume_init() };
    assert_eq!(
        unsafe { vfsi_pwritev_ex(fs, writes.as_mut_ptr(), 3, results.as_mut_ptr().cast()) }
            .category,
        VFSI_ERROR_NONE
    );
    let results = results.map(|r| unsafe { r.assume_init() });
    assert_eq!(results.map(|r| r.failure_state), [0, 3, 0]);
    assert_eq!(
        writes.iter().map(|r| r.wrote).collect::<Vec<_>>(),
        [2, 1, 2]
    );
    assert_eq!(results[1].offset_known, 0);
    // Invalid handles are per input and cannot suppress valid siblings.
    writes[1].fd = 999;
    let mut results = results;
    assert_eq!(
        unsafe { vfsi_pwritev_ex(fs, writes.as_mut_ptr(), 3, results.as_mut_ptr()) }.category,
        VFSI_ERROR_NONE
    );
    assert_eq!(results[1].failure_state, 1);
    assert_eq!(results[1].status.err_no, libc::EBADF as u32);
    unsafe {
        vfsi_free(fs);
    }
}

#[test]
fn extended_vectors_remap_rejections_after_invalid_handles() {
    for mode in [MalformedReply::ReadReject, MalformedReply::WriteReject] {
        let fs = independent_io_fixture(mode);
        let mut results = [std::mem::MaybeUninit::<vfsi_io_result>::uninit(); 2];
        let mut buffer = [0u8; 2];
        let result = if mode == MalformedReply::ReadReject {
            let mut ops = [
                vfsi_pread_op {
                    fd: 999,
                    buf: buffer.as_mut_ptr().cast(),
                    len: 1,
                    offset: 0,
                    got: 0,
                },
                vfsi_pread_op {
                    fd: 10,
                    buf: unsafe { buffer.as_mut_ptr().add(1) }.cast(),
                    len: 1,
                    offset: 0,
                    got: 0,
                },
            ];
            unsafe { vfsi_preadv_ex(fs, ops.as_mut_ptr(), 2, results.as_mut_ptr().cast()) }
        } else {
            let mut ops = [
                vfsi_pwrite_op {
                    fd: 999,
                    buf: buffer.as_ptr().cast(),
                    len: 1,
                    offset: 0,
                    wrote: 0,
                },
                vfsi_pwrite_op {
                    fd: 10,
                    buf: buffer.as_ptr().cast(),
                    len: 1,
                    offset: 0,
                    wrote: 0,
                },
            ];
            unsafe { vfsi_pwritev_ex(fs, ops.as_mut_ptr(), 2, results.as_mut_ptr().cast()) }
        };
        assert_eq!(result.category, VFSI_ERROR_UNSUPPORTED);
        assert_eq!(result.index, 1);
        unsafe {
            vfsi_free(fs);
        }
    }
}

#[test]
fn extended_read_panic_returns_unknown_and_poisoned_backend_rejects_future_io() {
    let fs = independent_io_fixture(MalformedReply::ReadPanic);
    let mut buffer = [0xa5u8; 2];
    let mut op = vfsi_pread_op {
        fd: 10,
        buf: buffer.as_mut_ptr().cast(),
        len: 2,
        offset: 0,
        got: 0,
    };
    let mut result = std::mem::MaybeUninit::<vfsi_io_result>::uninit();
    assert_eq!(
        unsafe { vfsi_preadv_ex(fs, &mut op, 1, result.as_mut_ptr()) }.category,
        VFSI_ERROR_NONE
    );
    let first = unsafe { result.assume_init() };
    assert_eq!(first.failure_state, VFSI_IO_UNKNOWN);
    assert_eq!(op.got, 0);
    assert_eq!(buffer, [0xa5; 2]);
    assert!(unsafe { &*fs }.fs.lock().is_err());
    assert_ne!(
        unsafe { vfsi_preadv_ex(fs, &mut op, 1, result.as_mut_ptr()) }.category,
        VFSI_ERROR_NONE
    );
    assert_eq!(
        unsafe { result.assume_init() }.failure_state,
        VFSI_IO_NOT_ATTEMPTED
    );
    unsafe {
        vfsi_free(fs);
    }
}

#[test]
fn extended_rejections_clear_all_stale_counts_before_dispatch() {
    let fs = independent_io_fixture(MalformedReply::ReadMiddleTransport);
    let mut buffers = [[0xa5u8; 2]; 3];
    let mut reads: Vec<_> = buffers
        .iter_mut()
        .enumerate()
        .map(|(i, b)| vfsi_pread_op {
            fd: 10 + i as i32,
            buf: if i == 1 {
                std::ptr::null_mut()
            } else {
                b.as_mut_ptr().cast()
            },
            len: 2,
            offset: 0,
            got: 99,
        })
        .collect();
    let mut statuses = [std::mem::MaybeUninit::<vfsi_io_result>::uninit(); 3];
    assert_eq!(
        unsafe { vfsi_preadv_ex(fs, reads.as_mut_ptr(), 3, statuses.as_mut_ptr().cast()) }.category,
        VFSI_ERROR_INVALID_ARGUMENT
    );
    assert!(reads.iter().all(|r| r.got == 0));
    assert_eq!(buffers, [[0xa5; 2]; 3]);
    assert!(statuses
        .iter()
        .all(|r| unsafe { r.assume_init() }.failure_state == VFSI_IO_NOT_ATTEMPTED));
    let mut writes: Vec<_> = (0..3)
        .map(|i| vfsi_pwrite_op {
            fd: 10 + i,
            buf: if i == 1 {
                std::ptr::null()
            } else {
                b"ab".as_ptr().cast()
            },
            len: 2,
            offset: 0,
            wrote: 99,
        })
        .collect();
    assert_eq!(
        unsafe { vfsi_pwritev_ex(fs, writes.as_mut_ptr(), 3, statuses.as_mut_ptr().cast()) }
            .category,
        VFSI_ERROR_INVALID_ARGUMENT
    );
    assert!(writes.iter().all(|r| r.wrote == 0));
    unsafe {
        vfsi_free(fs);
    }
}
