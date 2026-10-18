use std::path::{Path, PathBuf};
use vfsi_core::*;
use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::test_support::borrow_writes;
use vfsi_sync::*;

#[cfg(test)]
mod tests {
    use crate::checked_offset;

    use super::*;
    use proptest::prelude::*;

    fn offset_boundary() -> impl Strategy<Value = u64> {
        prop_oneof![
            0u64..=2048,
            (i64::MAX as u64 - 1024)..=(i64::MAX as u64 + 1024),
            (u64::MAX - 2048)..=u64::MAX,
        ]
    }

    proptest! {
        #[test]
        fn checked_offsets_match_u64_arithmetic_at_signed_and_unsigned_boundaries(
            base in offset_boundary(),
            delta in 0u64..=4096,
            index in 0usize..32,
        ) {
            match (base.checked_add(delta), checked_offset(base, delta, index)) {
                (Some(expected), Ok(actual)) => prop_assert_eq!(actual, expected),
                (None, Err(error)) => {
                    prop_assert_eq!(error.index(), Some(index));
                    prop_assert_eq!(error.raw_code(), libc::EOVERFLOW as u32);
                }
                (expected, actual) => prop_assert!(false, "expected {expected:?}, got {actual:?}"),
            }
        }
    }
    use crate::LocalBackend;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A unique temporary directory that is removed on drop.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(tag: &str) -> TempRoot {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!(
                "vnfs-vecfs-test-{}-{}-{}",
                tag,
                std::process::id(),
                n
            ));
            let _ = std::fs::remove_dir_all(&p);
            TempRoot(p)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A fresh dummy backend rooted at a unique temp directory.
    fn fs(tag: &str) -> (TempRoot, LocalBackend) {
        let root = TempRoot::new(tag);
        std::fs::create_dir(&root.0).unwrap();
        let fs = LocalBackend::new(root.0.clone(), ()).unwrap();
        (root, fs)
    }

    #[test]
    fn descriptor_identity_cache_stays_with_the_open_object_not_its_path() {
        let (root, mut backend) = fs("descriptor-identity");
        let path = root.0.join("file");
        std::fs::write(&path, b"old").unwrap();
        let handle = backend
            .open_raw_impl(Path::new("/file"), libc::O_RDONLY, 0)
            .unwrap();
        let open = backend.open_files.get(&handle.fd().unwrap()).unwrap();
        assert_eq!(open.identity.get(), None);
        let original = open.identity().unwrap();
        assert_eq!(open.identity.get(), Some(original));
        std::fs::rename(&path, root.0.join("old")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        assert_eq!(open.identity().unwrap(), original);
        use std::os::unix::fs::MetadataExt;
        let replacement = std::fs::metadata(path).unwrap();
        assert_ne!(original, (replacement.dev(), replacement.ino()));
        assert_eq!(open.file.metadata().unwrap().len(), 3);
        backend.close_impl(&handle).unwrap();
    }

    #[test]
    fn vfsi_vsetattrs_batches_permissions_and_sizes() {
        let (root, backend) = fs("vsetattrs-many");
        let client = FsClient::new(backend);
        let paths: Vec<_> = (0..64).map(|i| format!("/file-{i}")).collect();
        for path in &paths {
            std::fs::write(root.0.join(path.trim_start_matches('/')), b"original").unwrap();
        }
        let updates: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(i, path)| {
                vfsi_core::SetAttrsOp::new(path)
                    .permissions(Permissions::from_mode(0o640))
                    .len(i as u64)
            })
            .collect();
        vfsi_core::Vfsi::vsetattrs(&client, &updates).unwrap();
        use std::os::unix::fs::MetadataExt;
        for (i, path) in paths.iter().enumerate() {
            let metadata = std::fs::metadata(root.0.join(path.trim_start_matches('/'))).unwrap();
            assert_eq!(metadata.len(), i as u64);
            assert_eq!(metadata.mode() & 0o7777, 0o640);
        }
    }

    #[test]
    fn handle_attributes_survive_unlink_and_scalar_helpers_share_the_engine() {
        let (root, backend) = fs("unlinked-attrs");
        let client = FsClient::new(backend);
        std::fs::write(root.0.join("file"), b"original").unwrap();
        let file = client
            .open_options()
            .read(true)
            .write(true)
            .open("/file")
            .unwrap();
        std::fs::remove_file(root.0.join("file")).unwrap();
        std::fs::write(root.0.join("file"), b"replacement").unwrap();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_001);
        client
            .vsetattrs(&[vfsi_core::SetAttrsOp::new(vfsi_core::Target::File(&file))
                .len(3)
                .permissions(Permissions::from_mode(0o640))
                .modified(modified)
                .with_follow_final_symlink(false)])
            .unwrap();
        let attrs = client
            .attrs_with_options(
                vfsi_core::Target::file(&file),
                vfsi_core::api::AttrsOptions::new().with_fields(
                    vfsi_core::AttrMask::MODE
                        | vfsi_core::AttrMask::SIZE
                        | vfsi_core::AttrMask::MTIME,
                ),
            )
            .unwrap();
        assert_eq!(attrs.len(), Some(3));
        assert_eq!(attrs.modified(), Some(modified));
        assert_eq!(attrs.permissions().unwrap().mode() & 0o7777, 0o640);
        client.truncate(vfsi_core::Target::file(&file), 5).unwrap();
        client
            .chmod(
                vfsi_core::Target::file(&file),
                Permissions::from_mode(0o600),
            )
            .unwrap();
        assert_eq!(
            client
                .attrs(vfsi_core::Target::file(&file))
                .unwrap()
                .len()
                .unwrap(),
            5
        );
        assert_eq!(
            client
                .attrs(vfsi_core::Target::file(&file))
                .unwrap()
                .permissions()
                .unwrap()
                .mode()
                & 0o7777,
            0o600
        );
        assert_eq!(std::fs::read(root.0.join("file")).unwrap(), b"replacement");
        file.close().unwrap();
    }

    #[test]
    fn try_new_reports_setup_errors_instead_of_panicking() {
        let root = TempRoot::new("try-new-error");
        std::fs::create_dir(&root.0).unwrap();
        let not_a_directory = root.0.join("file");
        std::fs::write(&not_a_directory, b"not a directory").unwrap();
        let error = LocalBackend::new(not_a_directory.clone(), ())
            .err()
            .unwrap();
        assert_eq!(error.raw_code(), libc::ENOTDIR as u32);
        assert_eq!(error.path(), Some(not_a_directory.as_path()));
    }

    #[test]
    fn constructor_requires_an_existing_root_and_preserves_lookup_errors() {
        use std::os::unix::fs::PermissionsExt;
        let root = TempRoot::new("root-errors");
        std::fs::create_dir(&root.0).unwrap();
        let missing = root.0.join("missing/child");
        let error = LocalBackend::new(missing.clone(), ()).err().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(error.path(), Some(missing.as_path()));
        assert!(!root.0.join("missing").exists());

        let blocked = root.0.join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o0)).unwrap();
        let result = LocalBackend::new(blocked.clone(), ());
        std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Root bypasses directory permissions; unprivileged runs exercise EACCES.
        // SAFETY: geteuid has no arguments or preconditions.
        if unsafe { libc::geteuid() } != 0 {
            let error = result.err().unwrap();
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(error.path(), Some(blocked.as_path()));
        }
    }

    #[test]
    fn constructor_rejects_fifo_roots_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        let root = TempRoot::new("fifo-root");
        std::fs::create_dir(&root.0).unwrap();
        let fifo = root.0.join("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: path is a valid NUL-terminated filename in our temporary directory.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let (send, receive) = std::sync::mpsc::channel();
        let thread_path = fifo.clone();
        let worker = std::thread::spawn(move || {
            send.send(LocalBackend::new(thread_path, ()).err().unwrap())
                .unwrap();
        });
        let error = receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("opening a non-directory root must not block");
        worker.join().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
        assert_eq!(error.path(), Some(fifo.as_path()));
    }

    fn write(fs: &mut LocalBackend, path: &str, data: &[u8]) {
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::from_path(path),
            0,
            data.to_vec(),
        )
        .with_creation()]))
            .expect("write");
    }

    // ------------------------------------------------------------------
    // Offsets: no sentinel collision, Cur/End resolution, result offsets
    // ------------------------------------------------------------------

    #[test]
    fn absolute_offset_at_u64_max_minus_one_is_not_cur() {
        let (_root, mut fs) = fs("huge-offset");
        write(&mut fs, "/f", b"abcdefgh");
        let fd = fs.open_raw_impl(Path::new("/f"), 0, 0).unwrap();
        fs.seek_raw_impl(&fd, 2, SeekFrom::Set).unwrap();

        // Previously u64::MAX - 1 collided with the VF_OFFSET_CUR sentinel and
        // would have read from the current position (2) instead. With the
        // typed offset it is an absolute offset: the platform may reject it
        // (pread beyond i64::MAX) or return an empty read, but never data
        // from the current position.
        match fs.vread_impl(&[ReadOp::new(fd.clone(), VfOffset::At(u64::MAX - 1), 8)]) {
            Err(e) => assert!([ERR_INVAL, libc::EOVERFLOW as u32].contains(&e.raw_code())),
            Ok(r) => {
                assert!(r[0].data.is_empty());
                assert!(r[0].eof);
            }
        }
        fs.close_impl(&fd).unwrap();
    }

    #[test]
    fn cur_offset_reads_resolve_and_advance() {
        let (_root, mut fs) = fs("cur");
        write(&mut fs, "/f", b"hello world");
        let fd = fs.open_raw_impl(Path::new("/f"), libc::O_RDWR, 0).unwrap();
        assert_eq!(fs.seek_raw_impl(&fd, 0, SeekFrom::End).unwrap(), 11);

        let w = fs
            .vwrite_impl(&borrow_writes(&[WriteOp::new(
                fd.clone(),
                VfOffset::Cur,
                b"XY".to_vec(),
            )]))
            .unwrap();
        assert_eq!(w[0].offset, 11); // resolved current position
        let w = fs
            .vwrite_impl(&borrow_writes(&[WriteOp::new(
                fd.clone(),
                VfOffset::Cur,
                b"Z".to_vec(),
            )]))
            .unwrap();
        assert_eq!(w[0].offset, 13);

        let r = fs
            .vread_impl(&[ReadOp::new(fd.clone(), VfOffset::Cur, 100)])
            .unwrap();
        assert_eq!(r[0].offset, 14); // resolved, not the Cur sentinel
        assert!(r[0].data.is_empty());
        assert!(r[0].eof);
        fs.close_impl(&fd).unwrap();
    }

    #[test]
    fn borrowed_write_and_direct_read_into_preserve_offsets_and_eof() {
        let (_root, mut fs) = fs("borrowed-io");
        let file = FileOperand::from_path("/f");
        let write: WriteOp<&FileOperand, &[u8]> =
            WriteOp::new(&file, VfOffset::At(0), &b"hello"[..]).with_creation();
        let written = fs.vwrite_impl(&[write]).unwrap();
        assert_eq!(written[0].bytes_written, 5);

        let reads = [ReadOp::at(file.clone(), 0, 3), ReadOp::at(file, 3, 5)];
        let mut first = [0u8; 3];
        let mut second = [0u8; 5];
        let results = fs
            .vread_into_impl(&reads, &mut [&mut first, &mut second])
            .unwrap();
        assert_eq!(
            (results[0].offset, results[0].bytes_read, results[0].eof),
            (0, 3, false)
        );
        assert_eq!(
            (results[1].offset, results[1].bytes_read, results[1].eof),
            (3, 2, true)
        );
        assert_eq!(&first, b"hel");
        assert_eq!(&second[..2], b"lo");
    }

    #[test]
    fn end_offset_writes_append_and_reads_at_end() {
        let (_root, mut fs) = fs("end");
        write(&mut fs, "/f", b"hello world");

        let fd = fs.open_raw_impl(Path::new("/f"), libc::O_RDWR, 0).unwrap();
        let w = fs
            .vwrite_impl(&borrow_writes(&[WriteOp::new(
                fd.clone(),
                VfOffset::End,
                b"!".to_vec(),
            )]))
            .unwrap();
        assert_eq!(w[0].offset, 11);
        fs.close_impl(&fd).unwrap();

        // A read positioned at End starts at the file size, so it is at EOF.
        let r = fs
            .vread_impl(&[ReadOp::new(FileOperand::from_path("/f"), VfOffset::End, 5)])
            .unwrap();
        assert_eq!(r[0].offset, 12); // the resolved start is the file size
        assert!(r[0].data.is_empty());
        assert!(r[0].eof);

        let r = fs
            .vread_impl(&[ReadOp::new(
                FileOperand::from_path("/f"),
                VfOffset::End,
                100,
            )])
            .unwrap();
        assert!(r[0].eof);
        assert_eq!(fs.stat_impl(Path::new("/f")).unwrap().size, 12);
    }

    #[test]
    fn writev_truncate_removes_stale_tail() {
        let (_root, mut fs) = fs("writev-truncate");
        write(&mut fs, "/f", b"longer-than-needed");
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::from_path("/f"),
            0,
            b"hi".to_vec(),
        )
        .with_truncate()]))
            .unwrap();
        // O_TRUNC semantics: the stale tail is gone.
        assert_eq!(
            fs.read_raw_impl(&FileOperand::from_path("/f"), 0, 100)
                .unwrap(),
            b"hi"
        );

        // A plain overwrite keeps the tail (pwrite semantics).
        write(&mut fs, "/g", b"abcdef");
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::from_path("/g"),
            0,
            b"xy".to_vec(),
        )]))
        .unwrap();
        assert_eq!(
            fs.read_raw_impl(&FileOperand::from_path("/g"), 0, 100)
                .unwrap(),
            b"xycdef"
        );
    }

    #[test]
    fn eof_is_only_true_at_end() {
        let (_root, mut fs) = fs("eof");
        write(&mut fs, "/f", b"abc");

        let r = fs
            .vread_impl(&[ReadOp::at(FileOperand::from_path("/f"), 0, 3)])
            .unwrap();
        assert_eq!(r[0].data, b"abc");
        assert!(!r[0].eof);

        let r = fs
            .vread_impl(&[ReadOp::at(FileOperand::from_path("/f"), 0, 4)])
            .unwrap();
        assert_eq!(r[0].data, b"abc");
        assert!(r[0].eof);

        // Zero-length reads never report EOF.
        let r = fs
            .vread_impl(&[ReadOp::at(FileOperand::from_path("/f"), 0, 0)])
            .unwrap();
        assert!(r[0].data.is_empty());
        assert!(!r[0].eof);
    }

    #[test]
    fn fseek_takes_shared_ref_and_works() {
        let (_root, mut fs) = fs("fseek");
        write(&mut fs, "/f", b"hello world");
        let fd = fs.open_raw_impl(Path::new("/f"), 0, 0).unwrap();

        assert_eq!(fs.seek_raw_impl(&fd, 6, SeekFrom::Set).unwrap(), 6);
        let r = fs
            .vread_impl(&[ReadOp::new(fd.clone(), VfOffset::Cur, 5)])
            .unwrap();
        assert_eq!(r[0].data, b"world");

        assert_eq!(fs.seek_raw_impl(&fd, -5, SeekFrom::End).unwrap(), 6);
        assert_eq!(fs.seek_raw_impl(&fd, 0, SeekFrom::Cur).unwrap(), 6);
        assert_eq!(
            fs.seek_raw_impl(&fd, -100, SeekFrom::Set)
                .unwrap_err()
                .raw_code(),
            ERR_INVAL
        );
        fs.close_impl(&fd).unwrap();
    }

    #[test]
    fn offset_overflow_is_reported_without_io_or_cursor_wraparound() {
        let (root, mut fs) = fs("offset-overflow");
        write(&mut fs, "/f", b"x");
        let fd = fs.open_raw_impl(Path::new("/f"), libc::O_RDWR, 0).unwrap();
        fs.seek_raw_impl(&fd, 1, SeekFrom::Set).unwrap();

        let error = fs
            .vwrite_impl(&borrow_writes(&[WriteOp::at(
                fd.clone(),
                u64::MAX,
                b"xx".to_vec(),
            )]))
            .unwrap_err();
        assert_eq!(error.raw_code(), libc::EOVERFLOW as u32);
        assert_eq!(fs.seek_raw_impl(&fd, 0, SeekFrom::Cur).unwrap(), 1);
        assert_eq!(std::fs::metadata(root.0.join("f")).unwrap().len(), 1);
        assert_eq!(std::fs::read(root.0.join("f")).unwrap(), b"x");

        fs.seek_raw_impl(&fd, i64::MAX, SeekFrom::Set).unwrap();
        let error = fs.seek_raw_impl(&fd, 1, SeekFrom::Cur).unwrap_err();
        assert_eq!(error.raw_code(), libc::EOVERFLOW as u32);
        assert_eq!(fs.seek_raw_impl(&fd, 0, SeekFrom::Cur).unwrap(), i64::MAX);
        assert_eq!(std::fs::metadata(root.0.join("f")).unwrap().len(), 1);
        assert_eq!(std::fs::read(root.0.join("f")).unwrap(), b"x");
        fs.close_impl(&fd).unwrap();
    }

    #[test]
    fn adb_layout_overflow_fails_before_creating_the_file() {
        let (_root, mut fs) = fs("adb-overflow");
        let pattern = Adb::blocknum_only("/overflow", u64::MAX, 2, 2, 0, 0);
        let error = fs.vwrite_adb_impl(&[pattern]).unwrap_err();
        assert_eq!(error.raw_code(), libc::EOVERFLOW as u32);
        assert!(!fs.exists_impl(Path::new("/overflow")).unwrap());
    }

    // ------------------------------------------------------------------
    // FileOperand base/cwd resolution is honored by every path-taking method
    // ------------------------------------------------------------------

    #[test]
    fn cwd_relative_unlink_targets_cwd() {
        let (_root, mut fs) = fs("cwd-unlink");
        fs.mkdir_raw_impl(Path::new("/sub"), 0o755).unwrap();
        fs.chdir(Path::new("sub")).unwrap();

        write(&mut fs, "a", b"x"); // cwd-relative write
        fs.unlink_impl(Path::new("a")).unwrap();

        // The file was removed from sub/, not from the root.
        assert!(!fs.exists_impl(Path::new("a")).unwrap());
        assert_eq!(
            fs.lstat_impl(Path::new("/a")).unwrap_err().raw_code(),
            ERR_NOENT
        );
    }

    #[test]
    fn renamev_honors_path_base() {
        let (_root, mut fs) = fs("rename-base");
        fs.mkdir_raw_impl(Path::new("/sub"), 0o755).unwrap();
        write(&mut fs, "/src", b"1");
        write(&mut fs, "/sub/src2", b"2");
        fs.chdir(Path::new("sub")).unwrap();

        // base Abs with a non-slash path is root-relative even after chdir.
        let abs_src = FileOperand::Path {
            base: VfPathBase::Abs,
            path: PathBuf::from("src"),
        };
        let abs_dst = FileOperand::Path {
            base: VfPathBase::Abs,
            path: PathBuf::from("dst"),
        };
        fs.vrename_impl(&[(abs_src, abs_dst)]).unwrap();
        assert!(!fs.exists_impl(Path::new("/src")).unwrap());
        assert!(fs.exists_impl(Path::new("/dst")).unwrap());

        // base Cwd resolves against the cwd.
        let cwd_src = FileOperand::Path {
            base: VfPathBase::Cwd,
            path: PathBuf::from("src2"),
        };
        let cwd_dst = FileOperand::Path {
            base: VfPathBase::Cwd,
            path: PathBuf::from("dst2"),
        };
        fs.vrename_impl(&[(cwd_src, cwd_dst)]).unwrap();
        assert!(!fs.exists_impl(Path::new("/sub/src2")).unwrap());
        assert!(fs.exists_impl(Path::new("/sub/dst2")).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_replace_empty_vector_is_a_noop() {
        let (_root, mut fs) = fs("rename-noreplace-empty");
        assert!(
            fs.vrename_with_options_impl(&[], vfsi_core::api::RenameMode::NoReplace)
                .is_ok()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unsupported_noreplace_flag_is_distinguished_from_invalid_directory_move() {
        let unsupported = std::io::Error::from_raw_os_error(libc::EINVAL);
        assert_eq!(
            crate::noreplace_error_code(
                &unsupported,
                false,
                Path::new("/source"),
                Path::new("/target")
            ),
            VF_ERR_UNSUPPORTED,
        );
        assert_eq!(
            crate::noreplace_error_code(
                &unsupported,
                true,
                Path::new("/source"),
                Path::new("/source/child"),
            ),
            libc::EINVAL as u32,
        );
        let unsupported_operation = std::io::Error::from_raw_os_error(libc::EOPNOTSUPP);
        assert_eq!(
            crate::noreplace_error_code(
                &unsupported_operation,
                false,
                Path::new("/source"),
                Path::new("/target"),
            ),
            VF_ERR_UNSUPPORTED,
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_replace_preserves_invalid_self_directory_rename() {
        let (_root, mut fs) = fs("rename-noreplace-self");
        fs.mkdir_raw_impl(Path::new("/source"), 0o755).unwrap();
        fs.mkdir_raw_impl(Path::new("/source/child"), 0o755)
            .unwrap();
        let error = fs
            .vrename_with_options_impl(
                &[(
                    FileOperand::from_path("/source"),
                    FileOperand::from_path("/source/child/moved"),
                )],
                vfsi_core::api::RenameMode::NoReplace,
            )
            .unwrap_err();
        assert_eq!(error.raw_code(), libc::EINVAL as u32);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn no_replace_empty_vector_is_a_noop() {
        let (_root, mut fs) = fs("rename-noreplace-empty");
        assert!(
            fs.vrename_with_options_impl(&[], vfsi_core::api::RenameMode::NoReplace)
                .is_ok()
        );
    }

    #[test]
    fn vf_path_rejects_descriptors() {
        let (_root, mut fs) = fs("vf-path");
        write(&mut fs, "/f", b"x");
        let fd = fs.open_raw_impl(Path::new("/f"), 0, 0).unwrap();
        assert_eq!(fs.vf_path(&fd).unwrap_err().raw_code(), ERR_INVAL);
        fs.close_impl(&fd).unwrap();
    }

    #[test]
    fn vf_file_cwd_and_cwd_path_resolution() {
        let (_root, mut fs) = fs("cwd-variants");
        fs.mkdir_raw_impl(Path::new("/sub"), 0o755).unwrap();
        write(&mut fs, "/sub/f", b"x");

        // `cwd()` is the cwd itself; `cwd_path` is relative to it.
        assert_eq!(fs.vf_path(&FileOperand::cwd()).unwrap(), Path::new(""));
        assert_eq!(
            FileOperand::cwd_path("f").path(),
            Some(std::path::Path::new("f"))
        );
        fs.chdir(Path::new("/sub")).unwrap();
        assert_eq!(fs.vf_path(&FileOperand::cwd()).unwrap(), Path::new("sub"));
        assert_eq!(
            fs.vf_path(&FileOperand::cwd_path("f")).unwrap(),
            Path::new("sub/f")
        );

        // The cwd is a stat target but not a file for read/write.
        let mut a = VfAttrs {
            file: FileOperand::cwd(),
            masks: AttrMask::stat(),
            ..VfAttrs::default()
        };
        fs.vgetattrs_impl(std::slice::from_mut(&mut a)).unwrap();
        assert_eq!(a.ftype, VfType::Directory);
        assert_eq!(
            fs.vread_impl(&[ReadOp::new(FileOperand::cwd(), VfOffset::At(0), 1)])
                .unwrap_err()
                .raw_code(),
            ERR_ISDIR
        );
    }

    // ------------------------------------------------------------------
    // Dummy root sandbox: `..` and symlinks cannot escape the root
    // ------------------------------------------------------------------

    #[test]
    fn dummy_root_clamps_dotdot() {
        let (root, mut fs) = fs("sandbox-dotdot");

        // Writing through ".." lands inside the root, not in its parent.
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::from_path("/../escape"),
            0,
            b"x".to_vec(),
        )
        .with_creation()]))
            .unwrap();
        assert!(fs.exists_impl(Path::new("/escape")).unwrap());
        assert!(!root.0.parent().unwrap().join("escape").exists());

        // "/.." and "/../../x" stay under the root.
        let st = fs.stat_impl(Path::new("/..")).unwrap();
        assert_eq!(st.ftype, VfType::Directory);
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::from_path("/../sub1/../../sub2"),
            0,
            b"y".to_vec(),
        )
        .with_creation()]))
            .unwrap();
        assert!(fs.exists_impl(Path::new("/sub2")).unwrap());
        assert!(!root.0.parent().unwrap().join("sub2").exists());

        // A lexical "a/../b" path resolves to b.
        write(&mut fs, "/a", b"");
        fs.vrename_impl(&[(
            FileOperand::from_path("/a"),
            FileOperand::from_path("/x/../b"),
        )])
        .unwrap();
        assert!(fs.exists_impl(Path::new("/b")).unwrap());
        assert!(!fs.exists_impl(Path::new("/x")).unwrap());
    }

    #[test]
    fn dummy_root_resolves_absolute_symlink_targets_inside_root() {
        let (root, mut fs) = fs("sandbox-symlink");
        write(&mut fs, "/target", b"inside");

        // An absolute target is chroot-relative: "/target" is the root's
        // "target", so reads through the link work.
        fs.symlink_raw_impl(Path::new("/target"), Path::new("/abs-link"))
            .unwrap();
        assert_eq!(
            fs.read_raw_impl(&FileOperand::from_path("/abs-link"), 0, 6)
                .unwrap(),
            b"inside"
        );
        // ".." components in an absolute target are clamped at the root.
        fs.symlink_raw_impl(Path::new("/sub/../target"), Path::new("/dotdot-link"))
            .unwrap();
        assert_eq!(
            fs.read_raw_impl(&FileOperand::from_path("/dotdot-link"), 0, 6)
                .unwrap(),
            b"inside"
        );

        // An OS-absolute path (e.g. the root's parent) is treated as a
        // root-relative path: it cannot escape or touch the outside file.
        let outside = root.0.parent().unwrap().join("outside-target");
        std::fs::write(&outside, b"outside").unwrap();
        let inside_target = root.0.join(
            outside
                .strip_prefix("/")
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
        fs.symlink_raw_impl(Path::new(&outside), Path::new("/evil"))
            .unwrap();
        assert_eq!(
            fs.vread_impl(&[ReadOp::at(FileOperand::from_path("/evil"), 0, 8)])
                .unwrap_err()
                .raw_code(),
            ERR_NOENT,
            "resolves inside the root where nothing exists yet"
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");

        // Creating through a chroot-relative absolute target lands inside the
        // root.
        fs.mkdir_raw_impl(Path::new("/subdir"), 0o755).unwrap();
        fs.symlink_raw_impl(Path::new("/subdir/created-inside"), Path::new("/evil3"))
            .unwrap();
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::from_path("/evil3"),
            0,
            b"x".to_vec(),
        )
        .with_creation()]))
            .unwrap();
        assert_eq!(
            fs.read_raw_impl(&FileOperand::from_path("/subdir/created-inside"), 0, 1)
                .unwrap(),
            b"x"
        );

        // The link itself can still be inspected and removed (no-follow).
        assert_eq!(
            fs.lstat_impl(Path::new("/abs-link")).unwrap().ftype,
            VfType::Symlink
        );
        fs.readlink_raw_impl(Path::new("/abs-link")).unwrap();
        fs.unlink_impl(Path::new("/abs-link")).unwrap();

        // A dangling symlink to an absolute path still cannot touch the
        // outside of the root when creating through it.
        let dangling = root.0.parent().unwrap().join("never-created");
        fs.symlink_raw_impl(Path::new(&dangling), Path::new("/evil2"))
            .unwrap();
        assert_eq!(
            fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
                FileOperand::from_path("/evil2"),
                0,
                b"x".to_vec()
            )
            .with_creation()]))
                .unwrap_err()
                .raw_code(),
            ERR_NOENT,
            "the chroot-relative target's parent does not exist"
        );
        assert!(!dangling.exists());
        assert!(!inside_target.exists(), "nothing was created inside either");
        let _ = std::fs::remove_file(&outside);

        // A dangling relative symlink whose target is inside the root is
        // created through (POSIX O_CREAT semantics).
        fs.symlink_raw_impl(Path::new("internal-target"), Path::new("/ok-link"))
            .unwrap();
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::from_path("/ok-link"),
            0,
            b"z".to_vec(),
        )
        .with_creation()]))
            .unwrap();
        assert_eq!(
            fs.read_raw_impl(&FileOperand::from_path("/internal-target"), 0, 1)
                .unwrap(),
            b"z"
        );
    }

    #[test]
    fn no_follow_operations_reject_symlinked_parent_escape() {
        let (root, mut fs) = fs("sandbox-parent-link");
        let outside = TempRoot::new("sandbox-outside");
        std::fs::create_dir_all(&outside.0).unwrap();
        std::fs::write(outside.0.join("victim"), b"outside").unwrap();
        std::os::unix::fs::symlink("victim", outside.0.join("link")).unwrap();
        std::os::unix::fs::symlink(&outside.0, root.0.join("pivot")).unwrap();

        for error in [
            fs.vremove_impl(&[FileOperand::from_path("/pivot/victim")])
                .unwrap_err(),
            fs.vrename_impl(&[(
                FileOperand::from_path("/pivot/victim"),
                FileOperand::from_path("/renamed"),
            )])
            .unwrap_err(),
            fs.listdir_impl(Path::new("/pivot"), AttrMask::stat(), 0, false)
                .unwrap_err(),
            fs.vreadlink_impl(&[Path::new("/pivot/link")]).unwrap_err(),
            fs.vhardlink_impl(&[Path::new("/pivot/victim")], &[Path::new("/hard")])
                .unwrap_err(),
            fs.lstat_impl(Path::new("/pivot/victim")).unwrap_err(),
        ] {
            assert!([ERR_ACCES, ERR_NOENT].contains(&error.raw_code()));
        }
        assert_eq!(std::fs::read(outside.0.join("victim")).unwrap(), b"outside");
        assert!(!root.0.join("renamed").exists());
        assert!(!root.0.join("hard").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn anchored_no_follow_path_survives_parent_replacement() {
        let (root, fs) = fs("sandbox-parent-race");
        let outside = TempRoot::new("sandbox-race-outside");
        std::fs::create_dir_all(root.0.join("inside")).unwrap();
        std::fs::create_dir_all(&outside.0).unwrap();
        std::fs::write(root.0.join("inside/victim"), b"inside").unwrap();
        std::fs::write(outside.0.join("victim"), b"outside").unwrap();

        let anchored = fs.no_follow_path(&root.0.join("inside/victim")).unwrap();
        std::fs::rename(root.0.join("inside"), root.0.join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside.0, root.0.join("inside")).unwrap();

        std::fs::remove_file(&anchored).unwrap();
        assert!(!root.0.join("moved/victim").exists());
        assert_eq!(std::fs::read(outside.0.join("victim")).unwrap(), b"outside");
    }

    #[test]
    fn portable_fallback_create_keeps_parent_directory_anchored() {
        let (root, backend) = fs("sandbox-portable-parent-race");
        let outside = TempRoot::new("sandbox-portable-race-outside");
        std::fs::create_dir_all(root.0.join("inside")).unwrap();
        std::fs::create_dir_all(&outside.0).unwrap();

        let anchored = backend
            .no_follow_path_fallback(&root.0.join("inside/new"))
            .unwrap();
        std::fs::rename(root.0.join("inside"), root.0.join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside.0, root.0.join("inside")).unwrap();

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        LocalBackend::protect_create_open(&anchored, &mut options);
        options.open(&anchored).unwrap();

        assert!(root.0.join("moved/new").exists());
        assert!(!outside.0.join("new").exists());
    }

    #[test]
    fn directory_listing_pins_final_directory_before_enumeration() {
        let (root, backend) = fs("sandbox-directory-final-race");
        let outside = TempRoot::new("sandbox-directory-final-outside");
        std::fs::create_dir_all(root.0.join("inside")).unwrap();
        std::fs::create_dir_all(&outside.0).unwrap();
        std::fs::write(root.0.join("inside/internal-entry"), b"in").unwrap();
        std::fs::write(outside.0.join("outside-entry"), b"out").unwrap();

        let anchored = backend
            .open_directory_no_follow_final(&root.0.join("inside"))
            .unwrap();
        std::fs::rename(root.0.join("inside"), root.0.join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside.0, root.0.join("inside")).unwrap();

        let names: Vec<_> = std::fs::read_dir(&anchored)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from("internal-entry")]);
        assert!(
            backend
                .open_directory_no_follow_final(&root.0.join("inside"))
                .is_err(),
            "a symlink in the final directory component must be rejected"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn following_chown_rejects_a_missing_leaf_replaced_by_an_escaping_symlink() {
        use std::os::unix::fs::MetadataExt;
        let (root, fs) = fs("chown-missing-leaf-race");
        let outside = TempRoot::new("chown-outside");
        std::fs::create_dir_all(&outside.0).unwrap();
        let victim = outside.0.join("victim");
        std::fs::write(&victim, b"outside").unwrap();
        let before = std::fs::metadata(&victim).unwrap();
        let uid = if unsafe { libc::geteuid() } == 0 {
            10001
        } else {
            before.uid()
        };
        let update = VfAttrs {
            masks: AttrMask::UID,
            uid,
            ..VfAttrs::default()
        };
        let missing = root.0.join("missing");
        let anchored = fs.real_path(&missing).unwrap();
        assert!(anchored.nofollow_on_open);
        // Insert the leaf after resolution; the host kernel would follow this
        // absolute target outside the configured client root.
        std::os::unix::fs::symlink(&victim, &missing).unwrap();
        let error = LocalBackend::chown_path(&update, &anchored, true, 3).unwrap_err();
        assert_eq!(error.raw_code(), ERR_NOENT);
        assert_eq!(error.index(), Some(3));
        let after = std::fs::metadata(&victim).unwrap();
        assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
        assert_eq!(std::fs::read(&victim).unwrap(), b"outside");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn following_chown_keeps_an_existing_target_pinned_after_replacement() {
        use std::os::unix::fs::MetadataExt;
        let (root, fs) = fs("chown-existing-leaf-race");
        let outside = TempRoot::new("chown-existing-outside");
        std::fs::create_dir_all(&outside.0).unwrap();
        let victim = outside.0.join("victim");
        std::fs::write(&victim, b"outside").unwrap();
        let before = std::fs::metadata(&victim).unwrap();
        let uid = if unsafe { libc::geteuid() } == 0 {
            10001
        } else {
            before.uid()
        };
        let update = VfAttrs {
            masks: AttrMask::UID,
            uid,
            ..VfAttrs::default()
        };
        let original = root.0.join("file");
        let moved = root.0.join("moved");
        std::fs::write(&original, b"inside").unwrap();
        let anchored = fs.real_path(&original).unwrap();
        assert!(!anchored.nofollow_on_open);
        std::fs::rename(&original, &moved).unwrap();
        std::os::unix::fs::symlink(&victim, &original).unwrap();
        LocalBackend::chown_path(&update, &anchored, true, 0).unwrap();
        assert_eq!(std::fs::metadata(&moved).unwrap().uid(), uid);
        let after = std::fs::metadata(&victim).unwrap();
        assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn anchored_create_cannot_be_redirected_after_resolution() {
        let (root, fs) = fs("sandbox-create-race");
        let outside = TempRoot::new("sandbox-create-outside");
        std::fs::create_dir_all(root.0.join("inside")).unwrap();
        std::fs::create_dir_all(&outside.0).unwrap();
        std::fs::write(outside.0.join("new"), b"outside").unwrap();

        let anchored = fs.real_path(&root.0.join("inside/new")).unwrap();
        std::fs::rename(root.0.join("inside"), root.0.join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside.0, root.0.join("inside")).unwrap();
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        LocalBackend::protect_create_open(&anchored, &mut options);
        options.open(&anchored).unwrap();

        assert!(root.0.join("moved/new").exists());
        assert_eq!(std::fs::read(outside.0.join("new")).unwrap(), b"outside");
    }

    #[test]
    fn dummy_open_by_path_abs_is_root_relative() {
        let (_root, mut fs) = fs("open-abs");
        let fd = fs
            .open_path_impl(
                VfPathBase::Abs,
                Path::new("rel"),
                libc::O_CREAT | libc::O_RDWR,
                0o644,
            )
            .unwrap();
        fs.vwrite_impl(&borrow_writes(&[WriteOp::new(
            fd.clone(),
            VfOffset::At(0),
            b"x".to_vec(),
        )]))
        .unwrap();
        fs.close_impl(&fd).unwrap();
        assert!(fs.exists_impl(Path::new("/rel")).unwrap());
    }

    #[test]
    fn dummy_reports_special_file_types() {
        let (root, mut fs) = fs("special-types");
        let real = root.0.join("fifo");
        let c = std::ffi::CString::new(real.to_str().unwrap()).unwrap();
        unsafe { libc::mkfifo(c.as_ptr(), 0o644) };

        let sock_path = root.0.join("sock");
        let sock_c = std::ffi::CString::new(sock_path.to_str().unwrap()).unwrap();
        let fd = unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            let bytes = sock_c.as_bytes();
            for (i, b) in bytes.iter().take(107).enumerate() {
                addr.sun_path[i] = *b as libc::c_char;
            }
            libc::bind(
                fd,
                &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            );
            fd
        };
        assert!(fd >= 0);

        assert_eq!(
            fs.stat_impl(Path::new("/fifo")).unwrap().ftype,
            VfType::Fifo
        );
        assert_eq!(
            fs.lstat_impl(Path::new("/fifo")).unwrap().ftype,
            VfType::Fifo
        );
        assert_eq!(
            fs.stat_impl(Path::new("/sock")).unwrap().ftype,
            VfType::Socket
        );
        let listed = fs
            .listdir_impl(Path::new("/"), AttrMask::default(), 0, false)
            .unwrap();
        assert!(listed.iter().any(|e| e.ftype == VfType::Fifo));
        assert!(listed.iter().any(|e| e.ftype == VfType::Socket));

        unsafe { libc::close(fd) };
    }

    #[test]
    fn dummy_descriptor_sees_external_truncation() {
        let (root, mut fs) = fs("ext-trunc");
        write(&mut fs, "/f", b"0123456789");
        let fd = fs.open_raw_impl(Path::new("/f"), libc::O_RDWR, 0).unwrap();
        let real = root.0.join("f");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&real)
            .unwrap()
            .set_len(3)
            .unwrap();
        let r = fs
            .vread_impl(&[ReadOp::new(fd.clone(), VfOffset::At(0), 10)])
            .unwrap();
        assert_eq!(r[0].data, b"012", "descriptor sees the new size");
        fs.close_impl(&fd).unwrap();
    }

    #[test]
    fn dummy_cwd_dotdot_stays_in_root() {
        let (root, mut fs) = fs("cwd-dotdot");
        fs.mkdir_raw_impl(Path::new("/a"), 0o755).unwrap();
        fs.chdir(Path::new("/a")).unwrap();

        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::from_path("../x"),
            0,
            b"1".to_vec(),
        )
        .with_creation()]))
            .unwrap();
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::from_path("a/../y"),
            0,
            b"2".to_vec(),
        )
        .with_creation()]))
            .unwrap();
        assert!(fs.exists_impl(Path::new("/x")).unwrap());
        // From cwd /a, "a/../y" resolves to /a/y (the ".." cancels the "a").
        assert!(fs.exists_impl(Path::new("/a/y")).unwrap());
        assert!(!fs.exists_impl(Path::new("/y")).unwrap());
        assert!(!root.0.parent().unwrap().join("x").exists());
        assert!(!root.0.parent().unwrap().join("y").exists());

        // ".." from the root clamps at the root instead of escaping.
        fs.chdir(Path::new("/")).unwrap();
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::from_path("../z"),
            0,
            b"3".to_vec(),
        )
        .with_creation()]))
            .unwrap();
        assert!(fs.exists_impl(Path::new("/z")).unwrap());
        assert!(!root.0.parent().unwrap().join("z").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn to_eof_copy_between_hardlink_aliases_is_rejected_without_growth() {
        const CHILD: &str = "VFSI_LOCAL_TO_EOF_SELF_COPY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let (root, mut fs) = fs("self-copy-child");
            write(&mut fs, "/source", b"bounded-source");
            std::fs::hard_link(root.0.join("source"), root.0.join("alias")).unwrap();

            // Keep a regression to the old unbounded loop from filling the
            // test filesystem. The child exits on EFBIG/SIGXFSZ if it regresses.
            let limit = libc::rlimit {
                rlim_cur: 8 * 1024,
                rlim_max: 8 * 1024,
            };
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &limit) }, 0);
            let error = fs
                .vcopy_impl(
                    &[ExtentPair::new("/source", 0, "/alias", 1, None)],
                    CopyOptions::new(),
                )
                .unwrap_err();
            assert_eq!(error.raw_code(), ERR_INVAL);
            assert_eq!(
                std::fs::read(root.0.join("source")).unwrap(),
                b"bounded-source"
            );
            assert_eq!(
                std::fs::metadata(root.0.join("source")).unwrap().len(),
                b"bounded-source".len() as u64
            );
            return;
        }

        let test_name = std::thread::current().name().unwrap().to_owned();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(test_name)
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
            "child did not select exactly this test: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output.status.success(),
            "bounded self-copy child failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn finite_overlapping_copy_between_hardlink_aliases_is_rejected_before_mutation() {
        const CHUNK: usize = 1 << 20;
        let (root, mut fs) = fs("finite-self-copy");
        let original: Vec<u8> = (0..CHUNK * 2 + 16)
            .map(|index| (index % 251) as u8)
            .collect();
        std::fs::write(root.0.join("source"), &original).unwrap();
        std::fs::hard_link(root.0.join("source"), root.0.join("alias")).unwrap();

        let error = fs
            .vcopy_data_impl(&[ExtentPair::new(
                "/source",
                0,
                "/alias",
                1,
                Some((CHUNK + 16) as u64),
            )])
            .unwrap_err();
        assert_eq!(error.index(), Some(0));
        assert_eq!(error.raw_code(), ERR_INVAL);
        assert_eq!(std::fs::read(root.0.join("source")).unwrap(), original);
    }

    #[test]
    fn open_create_new_rejects_dangling_final_symlink() {
        let (root, mut fs) = fs("exclusive-dangling-symlink");
        fs.symlink_raw_impl(Path::new("missing-target"), Path::new("/dangling"))
            .unwrap();

        let error = fs
            .open_raw_impl(
                Path::new("/dangling"),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
            .unwrap_err();

        assert_eq!(error.raw_code(), libc::EEXIST as u32);
        assert!(std::fs::symlink_metadata(root.0.join("dangling")).is_ok());
        assert!(!root.0.join("missing-target").exists());
    }

    #[test]
    fn named_attr_syscalls_only_run_when_requested() {
        let (_root, mut fs) = fs("xattr-query-mask");
        write(&mut fs, "/f", b"x");

        crate::XATTR_QUERY_COUNT.with(|count| count.set(0));
        let mut attrs = VfAttrs {
            file: FileOperand::from_path("/f"),
            masks: AttrMask::MODE,
            ..VfAttrs::default()
        };
        fs.vgetattrs_impl(std::slice::from_mut(&mut attrs)).unwrap();
        assert_eq!(crate::XATTR_QUERY_COUNT.with(|count| count.get()), 0);
        assert!(!attrs.returned.contains(AttrMask::NAMED_ATTR));

        attrs.masks = AttrMask::NAMED_ATTR;
        fs.vgetattrs_impl(std::slice::from_mut(&mut attrs)).unwrap();
        assert_eq!(crate::XATTR_QUERY_COUNT.with(|count| count.get()), 1);
        assert!(attrs.returned.contains(AttrMask::NAMED_ATTR));
    }

    #[test]
    fn named_attr_query_errors_propagate_for_paths_and_handles() {
        let (_root, mut fs) = fs("xattr-query-error");
        write(&mut fs, "/f", b"x");
        let handle = fs
            .open_raw_impl(Path::new("/f"), libc::O_RDONLY, 0)
            .unwrap();

        for file in [FileOperand::from_path("/f"), handle.clone()] {
            let mut attrs = VfAttrs {
                file,
                masks: AttrMask::NAMED_ATTR,
                ..VfAttrs::default()
            };
            crate::XATTR_QUERY_ERROR.with(|error| error.set(Some(libc::EACCES)));
            let error = fs
                .vgetattrs_impl(std::slice::from_mut(&mut attrs))
                .unwrap_err();
            assert_eq!(error.raw_code(), libc::EACCES as u32);
            assert_eq!(error.index(), Some(0));
            assert!(!attrs.returned.contains(AttrMask::NAMED_ATTR));
        }

        fs.close_impl(&handle).unwrap();
    }

    #[test]
    fn dummy_named_attr_detection() {
        use std::ffi::CString;
        let (root, mut fs) = fs("xattr");
        write(&mut fs, "/f", b"x");
        let fd = fs
            .open_raw_impl(Path::new("/f"), libc::O_RDONLY, 0)
            .unwrap();
        let mut descriptor = VfAttrs {
            file: fd.clone(),
            masks: AttrMask::NAMED_ATTR,
            ..VfAttrs::default()
        };
        fs.vgetattrs_impl(std::slice::from_mut(&mut descriptor))
            .unwrap();
        assert!(!descriptor.has_named_attr);
        assert!(descriptor.returned.contains(AttrMask::NAMED_ATTR));
        let real = root.0.join("f");
        let real = real.to_string_lossy().into_owned();
        let c = CString::new(real).unwrap();
        let name = CString::new("user.test").unwrap();
        let val = b"v";
        let rc = unsafe {
            libc::setxattr(
                c.as_ptr(),
                name.as_ptr(),
                val.as_ptr() as *const libc::c_void,
                val.len(),
                0,
            )
        };
        assert_eq!(rc, 0, "setxattr");

        let mut a = VfAttrs {
            file: FileOperand::from_path("/f"),
            masks: AttrMask::NAMED_ATTR,
            ..VfAttrs::default()
        };
        fs.vgetattrs_impl(std::slice::from_mut(&mut a)).unwrap();
        assert!(a.has_named_attr);
        assert!(a.returned.contains(AttrMask::NAMED_ATTR));
        for unlink in [false, true] {
            if unlink {
                std::fs::remove_file(root.0.join("moved")).unwrap();
            } else {
                std::fs::rename(root.0.join("f"), root.0.join("moved")).unwrap();
                std::fs::write(root.0.join("f"), b"replacement").unwrap();
            }
            // Both metadata variants must inspect the opened object, including
            // after unlink, rather than the replacement at its diagnostic path.
            for follow in [true, false] {
                descriptor.has_named_attr = false;
                descriptor.returned = AttrMask::empty();
                if follow {
                    fs.vgetattrs_impl(std::slice::from_mut(&mut descriptor))
                        .unwrap();
                } else {
                    fs.vgetattrs_nofollow_impl(std::slice::from_mut(&mut descriptor))
                        .unwrap();
                }
                assert!(descriptor.has_named_attr);
                assert!(descriptor.returned.contains(AttrMask::NAMED_ATTR));
            }
            fs.vgetattrs_impl(std::slice::from_mut(&mut a)).unwrap();
            assert!(!a.has_named_attr);
            assert!(a.returned.contains(AttrMask::NAMED_ATTR));
        }
        fs.close_impl(&fd).unwrap();
    }

    // ------------------------------------------------------------------
    // Attributes: returned tracking, strict setattrsv, lsetattrsv
    // ------------------------------------------------------------------

    #[test]
    fn getattrsv_reports_returned_mask() {
        let (_root, mut fs) = fs("returned");
        write(&mut fs, "/f", b"x");

        let a = fs.stat_impl(Path::new("/f")).unwrap();
        assert_eq!(a.returned, AttrMask::stat());
        assert!(a.returned.contains(AttrMask::MODE));

        let mut a = VfAttrs {
            file: FileOperand::from_path("/f"),
            masks: AttrMask::MODE | AttrMask::SIZE | AttrMask::MTIME,
            ..VfAttrs::default()
        };
        fs.vgetattrs_impl(std::slice::from_mut(&mut a)).unwrap();
        assert_eq!(
            a.returned,
            AttrMask::MODE | AttrMask::SIZE | AttrMask::MTIME
        );
    }

    #[test]
    fn setattrsv_rejects_unsupported_bits() {
        use std::os::unix::fs::PermissionsExt;
        let (root, mut fs) = fs("setattr-strict");
        write(&mut fs, "/f", b"x");
        let path = root.0.join("f");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let mut a = VfAttrs {
            file: FileOperand::from_path("/f"),
            masks: AttrMask::BLOCKS,
            mode: 0o640,
            size: 0,
            ..VfAttrs::default()
        };
        for masks in [
            AttrMask::BLOCKS,
            AttrMask::MODE | AttrMask::SIZE | AttrMask::BLOCKS,
        ] {
            a.masks = masks;
            assert_eq!(
                fs.vsetattrs_raw_impl(std::slice::from_ref(&a))
                    .unwrap_err()
                    .raw_code(),
                VF_ERR_UNSUPPORTED
            );
            let metadata = std::fs::metadata(&path).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
            assert_eq!(metadata.len(), 1);
            assert_eq!(std::fs::read(&path).unwrap(), b"x");
        }

        // MODE-only still works.
        a.masks = AttrMask::MODE;
        a.mode = 0o640;
        fs.vsetattrs_raw_impl(std::slice::from_ref(&a)).unwrap();
        assert_eq!(fs.lstat_impl(Path::new("/f")).unwrap().mode & 0o7777, 0o640);
    }

    #[test]
    fn setattrsv_updates_access_and_modify_times() {
        let (_root, mut fs) = fs("setattr-times");
        write(&mut fs, "/f", b"contents");

        let update = VfAttrs {
            file: FileOperand::from_path("/f"),
            masks: AttrMask::ATIME | AttrMask::MTIME,
            atime_sec: 1_700_000_001,
            atime_nsec: 123_456_789,
            mtime_sec: 1_700_000_002,
            mtime_nsec: 987_654_321,
            ..VfAttrs::default()
        };
        fs.vsetattrs_raw_impl(std::slice::from_ref(&update))
            .unwrap();

        let mut actual = VfAttrs {
            file: FileOperand::from_path("/f"),
            masks: AttrMask::ATIME | AttrMask::MTIME | AttrMask::SIZE,
            ..VfAttrs::default()
        };
        fs.vgetattrs_impl(std::slice::from_mut(&mut actual))
            .unwrap();
        assert_eq!(
            (actual.atime_sec, actual.atime_nsec),
            (1_700_000_001, 123_456_789)
        );
        assert_eq!(
            (actual.mtime_sec, actual.mtime_nsec),
            (1_700_000_002, 987_654_321)
        );
        assert_eq!(actual.size, 8);
    }

    #[test]
    fn lsetattrsv_does_not_follow_final_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let (root, mut fs) = fs("lsetattr");
        write(&mut fs, "/target", b"x");
        let target = root.0.join("target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        fs.symlink_raw_impl(Path::new("/target"), Path::new("/link"))
            .unwrap();

        // No portable lchmod: the dummy backend refuses symlinks instead of
        // silently following them.
        let a = VfAttrs {
            file: FileOperand::from_path("/link"),
            masks: AttrMask::MODE,
            mode: 0o600,
            ..VfAttrs::default()
        };
        assert_eq!(
            fs.vsetattrs_raw_nofollow_impl(std::slice::from_ref(&a))
                .unwrap_err()
                .raw_code(),
            VF_ERR_UNSUPPORTED
        );
        let metadata = std::fs::metadata(&target).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o640);
        assert_eq!(metadata.len(), 1);
        assert_eq!(std::fs::read(&target).unwrap(), b"x");
        assert!(
            std::fs::symlink_metadata(root.0.join("link"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_link(root.0.join("link")).unwrap(),
            Path::new("/target")
        );

        // Regular files are set normally.
        let a = VfAttrs {
            file: FileOperand::from_path("/target"),
            masks: AttrMask::MODE,
            mode: 0o600,
            ..VfAttrs::default()
        };
        fs.vsetattrs_raw_nofollow_impl(std::slice::from_ref(&a))
            .unwrap();
        assert_eq!(
            fs.lstat_impl(Path::new("/target")).unwrap().mode & 0o7777,
            0o600
        );
    }

    // ------------------------------------------------------------------
    // exists / file_type use lstat semantics
    // ------------------------------------------------------------------

    #[test]
    fn exists_and_file_type_use_lstat_semantics() {
        let (_root, mut fs) = fs("lstat");
        write(&mut fs, "/f", b"x");
        fs.symlink_raw_impl(Path::new("missing-target"), Path::new("/dangling"))
            .unwrap();

        assert!(fs.exists_impl(Path::new("/dangling")).unwrap());
        assert_eq!(
            fs.file_type_impl(Path::new("/dangling")).unwrap(),
            VfType::Symlink
        );
        assert_eq!(fs.file_type_impl(Path::new("/f")).unwrap(), VfType::Regular);
    }

    // ------------------------------------------------------------------
    // openv length contract, listdir limits, walk via dyn VectorBackend
    // ------------------------------------------------------------------

    #[test]
    fn openv_rejects_mismatched_lengths() {
        let (_root, mut fs) = fs("openv");
        use libc::O_CREAT;
        let e = VectorBackend::vopen_raw_impl(
            &mut fs,
            &[Path::new("/a"), Path::new("/b")],
            &[O_CREAT],
            &[0o644],
        )
        .unwrap_err();
        assert_eq!((e.index(), e.raw_code()), (Some(0), ERR_INVAL));
    }

    #[test]
    fn listdir_zero_max_count_is_unlimited() {
        let (_root, mut fs) = fs("listdir");
        fs.mkdir_raw_impl(Path::new("/d"), 0o755).unwrap();
        write(&mut fs, "/d/a", b"1");
        write(&mut fs, "/d/b", b"2");

        let all = fs
            .listdir_impl(Path::new("/d"), AttrMask::default(), 0, false)
            .unwrap();
        assert_eq!(all.len(), 2);
        let one = fs
            .listdir_impl(Path::new("/d"), AttrMask::default(), 1, false)
            .unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn walk_works_through_dyn_vecfs() {
        let (_root, mut fs) = fs("walk-dyn");
        fs.mkdir_raw_impl(Path::new("/sub"), 0o755).unwrap();
        write(&mut fs, "/sub/a", b"1");

        let mut dyn_fs: Box<dyn VectorBackend> = Box::new(fs);
        let mut visited: Vec<String> = Vec::new();
        let entries = dyn_fs
            .walk_impl(Path::new(""), AttrMask::stat(), &mut |dir, _| {
                visited.push(dir.display().to_string())
            })
            .unwrap();
        assert_eq!(visited.len(), 2); // root + /sub
        assert_eq!(entries.len(), 2);
        let sub = entries.iter().find(|w| w.path.ends_with("sub")).unwrap();
        assert_eq!(sub.entries.len(), 1);
        assert_eq!(sub.entries[0].ftype, VfType::Regular);
    }

    #[test]
    fn copy_options_preserve_source_symlinks() {
        let (_root, mut fs) = fs("copy-options");
        write(&mut fs, "/target", b"data");
        fs.symlink_raw_impl(Path::new("target"), Path::new("/link"))
            .unwrap();

        let pair = ExtentPair::new("/link", 0, "/link-copy", 0, None);
        fs.vcopy_impl(
            std::slice::from_ref(&pair),
            vfsi_core::CopyOptions::new().with_follow_source_symlinks(false),
        )
        .unwrap();
        assert_eq!(
            fs.file_type_impl(Path::new("/link-copy")).unwrap(),
            VfType::Symlink
        );
        assert_eq!(
            fs.readlink_raw_impl(Path::new("/link-copy")).unwrap(),
            fs.readlink_raw_impl(Path::new("/link")).unwrap()
        );

        // The default option on the same entry point copies the target's data.
        let pair = ExtentPair::new("/link", 0, "/link-dup", 0, None);
        fs.vcopy_impl(std::slice::from_ref(&pair), CopyOptions::new())
            .unwrap();
        assert_eq!(
            fs.file_type_impl(Path::new("/link-dup")).unwrap(),
            VfType::Regular
        );
        assert_eq!(
            fs.read_raw_impl(&FileOperand::from_path("/link-dup"), 0, 4)
                .unwrap(),
            b"data"
        );
    }

    #[test]
    fn copy_options_handle_mixed_dangling_links_and_original_error_indices() {
        let (_root, mut fs) = fs("copy-options-mixed");
        write(&mut fs, "/source", b"0123456789");
        fs.symlink_raw_impl(Path::new("missing-target"), Path::new("/dangling"))
            .unwrap();
        let options = CopyOptions::new().with_follow_source_symlinks(false);
        let pairs = [
            ExtentPair::new("/source", 2, "/data-copy", 0, Some(4)),
            ExtentPair::new("/dangling", 99, "/dangling-copy", 100, Some(0)),
        ];
        fs.vcopy_impl(&pairs, options).unwrap();
        assert_eq!(
            fs.read_raw_impl(&FileOperand::from_path("/data-copy"), 0, 4)
                .unwrap(),
            b"2345"
        );
        assert_eq!(
            fs.readlink_raw_impl(Path::new("/dangling-copy")).unwrap(),
            b"missing-target"
        );
        let error = fs
            .vcopy_impl(
                &[
                    ExtentPair::new("/source", 0, "/prefix", 0, None),
                    pairs[1].clone(),
                ],
                options,
            )
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert_eq!(error.raw_code(), libc::EEXIST as u32);
        // Following a dangling link fails, while preserving it succeeds above.
        let error = fs
            .vcopy_impl(
                &[ExtentPair::new("/dangling", 0, "/followed", 0, None)],
                CopyOptions::new(),
            )
            .unwrap_err();
        assert_eq!(error.raw_code(), libc::ENOENT as u32);
        fs.vcopy_impl(&[], options).unwrap();
    }

    #[test]
    fn shared_tree_copy_preserves_symlinks_and_uses_the_copy_strategy() {
        use std::os::unix::fs::PermissionsExt;

        let (root, mut backend) = fs("shared-tree-copy");
        std::fs::create_dir_all(root.0.join("source/sub")).unwrap();
        std::fs::write(root.0.join("source/file"), b"root").unwrap();
        std::fs::write(root.0.join("source/sub/leaf"), b"child").unwrap();
        std::os::unix::fs::symlink("file", root.0.join("source/link")).unwrap();
        let mut copied = Vec::new();
        vfsi_sync::backend::helpers::copy_tree_with(
            &mut backend,
            Path::new("/source"),
            Path::new("/copy"),
            true,
            AttrMask::MODE | AttrMask::SIZE,
            |backend, pair| {
                copied.push(pair.src_path.clone());
                backend.vcopy_data_impl(std::slice::from_ref(pair))
            },
            std::convert::identity,
        )
        .unwrap();
        for directory in ["copy", "copy/sub"] {
            assert_eq!(
                std::fs::metadata(root.0.join(directory))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755,
                "POSIX recursive-copy defaults must not change"
            );
        }
        assert_eq!(
            copied,
            [Path::new("/source/file"), Path::new("/source/sub/leaf")]
        );
        assert_eq!(
            std::fs::read(root.0.join("copy/sub/leaf")).unwrap(),
            b"child"
        );
        assert_eq!(
            std::fs::read_link(root.0.join("copy/link")).unwrap(),
            Path::new("file")
        );
        for (destination, reindex) in [("/failed", false), ("/indexed", true)] {
            let mut calls = 0;
            let error = vfsi_sync::backend::helpers::copy_tree_with(
                &mut backend,
                Path::new("/source"),
                Path::new(destination),
                true,
                AttrMask::MODE,
                |_, _| {
                    calls += 1;
                    Err(VfError::transport(None, "lost reply"))
                },
                |error| if reindex { error.with_index(0) } else { error },
            )
            .unwrap_err();
            assert_eq!(calls, 1, "never replay a failed copy strategy");
            assert!(error.is_transport());
            assert_eq!(error.index(), reindex.then_some(0));
        }
    }

    #[test]
    fn deep_recursive_operations_use_bounded_call_stack() {
        const DEPTH: usize = 384;
        let (_root, mut fs) = fs("deep-iterative");
        fs.mkdir_raw_impl(Path::new("/source"), 0o755).unwrap();
        let mut directory = PathBuf::from("/source");
        for _ in 0..DEPTH {
            directory.push("d");
            fs.mkdir_raw_impl(&directory, 0o755).unwrap();
        }
        let leaf = directory.join("leaf");
        fs.vwrite_impl(&borrow_writes(&[WriteOp::from_os_path(
            &leaf,
            VfOffset::At(0),
            b"deep".to_vec(),
        )
        .with_creation()]))
            .unwrap();

        let listed = fs
            .listdir_impl(Path::new("/source"), AttrMask::MODE, 0, true)
            .unwrap();
        assert_eq!(listed.len(), DEPTH + 1);
        fs.copy_tree_impl(Path::new("/source"), Path::new("/copy"), false, false)
            .unwrap();
        let copied_leaf = Path::new("/copy").join(
            leaf.strip_prefix("/source")
                .expect("leaf remains below source"),
        );
        assert_eq!(
            fs.read_raw_impl(&FileOperand::from_os_path(&copied_leaf), 0, 4)
                .unwrap(),
            b"deep"
        );
        fs.remove_paths_impl(&[Path::new("/source"), Path::new("/copy")], true)
            .unwrap();
        assert!(!fs.exists_impl(Path::new("/source")).unwrap());
        assert!(!fs.exists_impl(Path::new("/copy")).unwrap());
    }
    #[test]
    fn native_scalar_adapters_preserve_application_error_context() {
        let (_root, backend) = fs("native-error-context");
        let client = vfsi_sync::FsClient::new(backend);
        let error = client.open("/missing").unwrap_err();
        assert_eq!(error.operation(), Some("open"));
        assert_eq!(error.path(), Some(Path::new("/missing")));
        assert_eq!(error.index(), Some(0));
        let error = client.attrs("/missing").unwrap_err();
        assert_eq!(error.operation(), Some("attrs"));
        assert_eq!(error.path(), Some(Path::new("/missing")));
        assert_eq!(error.index(), Some(0));
    }
    #[cfg(feature = "test-faults")]
    struct ReplaceLeafWithSymlink {
        leaf: PathBuf,
        target: PathBuf,
    }

    #[cfg(feature = "test-faults")]
    impl vfsi_core::internal::faults::FaultInjector for ReplaceLeafWithSymlink {
        fn check(&self, point: &vfsi_core::internal::faults::OpenFaultPoint) -> VfResult<()> {
            if matches!(
                point,
                vfsi_core::internal::faults::OpenFaultPoint::BeforeSetPermissions { index: 0 }
            ) {
                if let Ok(metadata) = std::fs::symlink_metadata(&self.leaf) {
                    if metadata.file_type().is_dir() {
                        std::fs::remove_dir(&self.leaf).unwrap();
                    } else {
                        std::fs::remove_file(&self.leaf).unwrap();
                    }
                }
                std::os::unix::fs::symlink(&self.target, &self.leaf).unwrap();
            }
            Ok(())
        }
    }

    #[cfg(feature = "test-faults")]
    struct ReplaceLeafWithSymlinkAfterNoFollowCheck {
        leaf: PathBuf,
        target: PathBuf,
    }

    #[cfg(feature = "test-faults")]
    impl vfsi_core::internal::faults::FaultInjector for ReplaceLeafWithSymlinkAfterNoFollowCheck {
        fn check(&self, point: &vfsi_core::internal::faults::OpenFaultPoint) -> VfResult<()> {
            if matches!(
                point,
                vfsi_core::internal::faults::OpenFaultPoint::AfterNoFollowSetattrTypeCheck {
                    index: 0
                }
            ) {
                std::fs::remove_file(&self.leaf).unwrap();
                std::os::unix::fs::symlink(&self.target, &self.leaf).unwrap();
            }
            Ok(())
        }
    }

    #[cfg(all(feature = "test-faults", unix))]
    #[test]
    fn nofollow_setattrs_do_not_mutate_a_leaf_symlink_swap_target() {
        use std::os::unix::fs::PermissionsExt;

        let (root, mut backend) = fs("nofollow-setattr-symlink-swap");
        write(&mut backend, "/target", b"protected");
        std::fs::set_permissions(
            root.0.join("target"),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        let target = root.0.join("target");
        let original_mtime = std::fs::metadata(&target).unwrap().modified().unwrap();

        for (index, (mask, mode, size, mtime_sec)) in [
            (AttrMask::MODE, 0o600, 0, 0),
            (AttrMask::SIZE, 0, 1, 0),
            (AttrMask::MTIME, 0, 0, 1_700_000_000),
        ]
        .into_iter()
        .enumerate()
        {
            let leaf_name = format!("leaf-{index}");
            write(&mut backend, &format!("/{leaf_name}"), b"original");
            let leaf = root.0.join(&leaf_name);
            backend.set_fault_injector(std::sync::Arc::new(
                ReplaceLeafWithSymlinkAfterNoFollowCheck {
                    leaf: leaf.clone(),
                    target: PathBuf::from("target"),
                },
            ));

            let virtual_path = format!("/{leaf_name}");
            let attrs = VfAttrs {
                file: FileOperand::from_path(&virtual_path),
                masks: mask,
                mode,
                size,
                mtime_sec,
                ..VfAttrs::default()
            };
            assert!(
                backend
                    .vsetattrs_raw_nofollow_impl(std::slice::from_ref(&attrs))
                    .is_err(),
                "nofollow update {mask:?} should reject the replaced symlink"
            );
            assert_eq!(std::fs::read(&target).unwrap(), b"protected");
            let metadata = std::fs::metadata(&target).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o7777, 0o640);
            assert_eq!(metadata.modified().unwrap(), original_mtime);
            assert!(
                std::fs::symlink_metadata(leaf)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[cfg(all(feature = "test-faults", unix))]
    #[test]
    fn mkdir_mode_does_not_follow_a_leaf_symlink_swap() {
        use std::os::unix::fs::PermissionsExt;

        let (root, mut backend) = fs("mkdir-mode-symlink-swap");
        let outside = TempRoot::new("mkdir-mode-symlink-target");
        std::fs::create_dir_all(&outside.0).unwrap();
        let target = outside.0.join("target-dir");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let leaf = root.0.join("new-dir");
        backend.set_fault_injector(std::sync::Arc::new(ReplaceLeafWithSymlink {
            leaf: leaf.clone(),
            target: target.clone(),
        }));

        let attrs = VfAttrs {
            file: FileOperand::from_path("/new-dir"),
            masks: AttrMask::MODE,
            mode: 0o700,
            ..VfAttrs::default()
        };
        assert!(backend.vmkdir_impl(std::slice::from_ref(&attrs)).is_err());
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o640
        );
        assert!(
            std::fs::symlink_metadata(leaf)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(all(feature = "test-faults", unix))]
    #[test]
    fn setattr_mode_does_not_follow_a_leaf_symlink_swap() {
        use std::os::unix::fs::PermissionsExt;

        let (root, mut backend) = fs("setattr-mode-symlink-swap");
        let outside = TempRoot::new("setattr-mode-symlink-target");
        std::fs::create_dir_all(&outside.0).unwrap();
        let target = outside.0.join("target");
        std::fs::write(&target, b"outside").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let leaf = root.0.join("new-file");
        backend.set_fault_injector(std::sync::Arc::new(ReplaceLeafWithSymlink {
            leaf: leaf.clone(),
            target: target.clone(),
        }));

        let attrs = VfAttrs {
            file: FileOperand::from_path("/new-file"),
            masks: AttrMask::MODE,
            mode: 0o600,
            ..VfAttrs::default()
        };
        assert!(
            backend
                .vsetattrs_raw_impl(std::slice::from_ref(&attrs))
                .is_err()
        );
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o640
        );
        assert!(
            std::fs::symlink_metadata(leaf)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_create_honors_umask_and_does_not_chmod_existing_file() {
        const CHILD: &str = "VFSI_LOCAL_OPEN_UMASK_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let test_name = std::thread::current().name().unwrap().to_owned();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(test_name)
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
                "child did not select exactly this test: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(
                output.status.success(),
                "open umask child failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        use std::os::unix::fs::PermissionsExt;

        let (root, mut fs) = fs("open-create-umask");
        std::fs::write(root.0.join("existing"), b"old").unwrap();
        std::fs::set_permissions(
            root.0.join("existing"),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();

        let previous = unsafe { libc::umask(0o077) };
        let result = (|| {
            let created =
                fs.open_raw_impl(Path::new("/created"), libc::O_WRONLY | libc::O_CREAT, 0o666)?;
            fs.close_impl(&created)?;
            let existing =
                fs.open_raw_impl(Path::new("/existing"), libc::O_RDWR | libc::O_CREAT, 0o777)?;
            fs.close_impl(&existing)
        })();
        unsafe { libc::umask(previous) };
        result.unwrap();

        assert_eq!(
            std::fs::metadata(root.0.join("created"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(root.0.join("existing"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o640
        );
    }

    #[cfg(feature = "test-faults")]
    fn run_filtered_test_child(test_name: &str, marker: &str) {
        use std::process::Command;
        use std::time::{Duration, Instant};

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name])
            .env(marker, "1")
            .spawn()
            .unwrap();
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "isolated test child failed: {status}");
                return;
            }
            if started.elapsed() >= Duration::from_secs(5) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("isolated test child timed out: {test_name}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(all(feature = "test-faults", target_os = "linux"))]
    #[test]
    fn mkdir_mode_overrides_restrictive_umask() {
        const CHILD: &str = "VFSI_LOCAL_UMASK_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            run_filtered_test_child(
                "contract_tests::tests::mkdir_mode_overrides_restrictive_umask",
                CHILD,
            );
            return;
        }

        use std::os::unix::fs::PermissionsExt;

        let (root, mut backend) = fs("mkdir-restrictive-umask");
        let previous = unsafe { libc::umask(0o700) };
        let attrs = VfAttrs {
            file: FileOperand::from_path("/restricted"),
            masks: AttrMask::MODE,
            mode: 0o700,
            ..VfAttrs::default()
        };
        let result = backend.vmkdir_impl(std::slice::from_ref(&attrs));
        unsafe { libc::umask(previous) };
        result.unwrap();
        assert_eq!(
            std::fs::metadata(root.0.join("restricted"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
    }

    #[cfg(feature = "test-faults")]
    struct ReplaceLeafWithFifo {
        leaf: PathBuf,
    }

    #[cfg(feature = "test-faults")]
    impl vfsi_core::internal::faults::FaultInjector for ReplaceLeafWithFifo {
        fn check(&self, point: &vfsi_core::internal::faults::OpenFaultPoint) -> VfResult<()> {
            if matches!(
                point,
                vfsi_core::internal::faults::OpenFaultPoint::BeforeSetPermissions { index: 0 }
            ) {
                use std::ffi::CString;
                use std::os::unix::ffi::OsStrExt;

                let path = CString::new(self.leaf.as_os_str().as_bytes())
                    .map_err(|_| VfError::failure(0, ERR_INVAL))?;
                if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
                    return Err(VfError::client(
                        0,
                        LocalBackend::errno(&std::io::Error::last_os_error()),
                    ));
                }
            }
            Ok(())
        }
    }

    #[cfg(all(feature = "test-faults", unix))]
    #[test]
    fn setattr_mode_does_not_block_on_a_raced_fifo() {
        const CHILD: &str = "VFSI_LOCAL_FIFO_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            run_filtered_test_child(
                "contract_tests::tests::setattr_mode_does_not_block_on_a_raced_fifo",
                CHILD,
            );
            return;
        }

        use std::os::unix::fs::FileTypeExt;

        let (root, mut backend) = fs("setattr-mode-fifo-swap");
        let leaf = root.0.join("new-file");
        backend.set_fault_injector(std::sync::Arc::new(ReplaceLeafWithFifo {
            leaf: leaf.clone(),
        }));
        let attrs = VfAttrs {
            file: FileOperand::from_path("/new-file"),
            masks: AttrMask::MODE,
            mode: 0o600,
            ..VfAttrs::default()
        };
        backend
            .vsetattrs_raw_impl(std::slice::from_ref(&attrs))
            .unwrap();
        assert!(
            std::fs::symlink_metadata(leaf)
                .unwrap()
                .file_type()
                .is_fifo()
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn directory_cursor_metadata_stays_with_renamed_directory() {
        use std::os::unix::fs::MetadataExt;
        for replace in [true, false] {
            let (root, mut backend) = fs("cursor-metadata-identity");
            std::fs::create_dir(root.0.join("tree")).unwrap();
            let mut expected = std::collections::HashMap::new();
            for name in ["a", "b", "c"] {
                let path = root.0.join("tree").join(name);
                std::fs::write(&path, b"original").unwrap();
                expected.insert(name.to_owned(), std::fs::metadata(&path).unwrap().ino());
                let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: the path/name and one-byte value remain valid during
                // this synchronous syscall and contain the required terminators.
                let result = unsafe {
                    libc::setxattr(
                        path.as_ptr(),
                        c"user.prr_cursor".as_ptr(),
                        b"x".as_ptr().cast(),
                        1,
                        0,
                    )
                };
                assert_eq!(result, 0);
            }
            let fields = AttrMask::FILEID | AttrMask::NAMED_ATTR;
            let (first, cursor) = backend
                .listdir_page_impl(Path::new("/tree"), fields, None, 1, 10, true)
                .unwrap();
            assert_eq!(first.len(), 1);
            assert!(first[0].has_named_attr);
            let cursor = cursor.expect("more entries remain");
            std::fs::rename(root.0.join("tree"), root.0.join("moved")).unwrap();
            if replace {
                std::fs::create_dir(root.0.join("tree")).unwrap();
                for name in ["a", "b", "c"] {
                    std::fs::write(root.0.join("tree").join(name), b"replacement").unwrap();
                }
            }
            let (rest, _) = backend
                .listdir_page_impl(Path::new("/tree"), fields, Some(cursor), 128, 10, true)
                .unwrap();
            assert_eq!(rest.len(), 2);
            for entry in rest {
                let name = entry
                    .file
                    .path()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap();
                assert_eq!(entry.fileid, expected[name]);
                assert!(
                    entry.has_named_attr,
                    "xattrs must describe the retained original entry"
                );
                assert!(entry.returned.contains(AttrMask::NAMED_ATTR));
            }
        }
    }
}
