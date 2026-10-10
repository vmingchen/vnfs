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
                    prop_assert_eq!(error.err_no(), libc::EOVERFLOW as u32);
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
        let fs = LocalBackend::new(root.0.clone(), ()).unwrap();
        (root, fs)
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
                .follow_symlinks(false)])
            .unwrap();
        let attrs = client
            .attrs_with_options(
                vfsi_core::Target::file(&file),
                vfsi_core::api::AttrsOptions::new().fields(
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
        let error = LocalBackend::new(not_a_directory, ()).err().unwrap();
        assert!(matches!(
            error.err_no(),
            value if value == libc::EEXIST as u32 || value == libc::ENOTDIR as u32
        ));
    }

    fn write(fs: &mut LocalBackend, path: &str, data: &[u8]) {
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            VfFile::from_path(path),
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
            Err(e) => assert!([ERR_INVAL, libc::EOVERFLOW as u32].contains(&e.err_no())),
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
        let file = VfFile::from_path("/f");
        let write: WriteOp<&VfFile, &[u8]> =
            WriteOp::new(&file, VfOffset::At(0), &b"hello"[..]).with_creation();
        let written = fs.vwrite_impl(&[write]).unwrap();
        assert_eq!(written[0].written, 5);

        let reads = [ReadOp::at(file.clone(), 0, 3), ReadOp::at(file, 3, 5)];
        let mut first = [0u8; 3];
        let mut second = [0u8; 5];
        let results = fs
            .vread_into_impl(&reads, &mut [&mut first, &mut second])
            .unwrap();
        assert_eq!(
            (results[0].offset, results[0].read, results[0].eof),
            (0, 3, false)
        );
        assert_eq!(
            (results[1].offset, results[1].read, results[1].eof),
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
            .vread_impl(&[ReadOp::new(VfFile::from_path("/f"), VfOffset::End, 5)])
            .unwrap();
        assert_eq!(r[0].offset, 12); // the resolved start is the file size
        assert!(r[0].data.is_empty());
        assert!(r[0].eof);

        let r = fs
            .vread_impl(&[ReadOp::new(VfFile::from_path("/f"), VfOffset::End, 100)])
            .unwrap();
        assert!(r[0].eof);
        assert_eq!(fs.stat_impl(Path::new("/f")).unwrap().size, 12);
    }

    #[test]
    fn writev_truncate_removes_stale_tail() {
        let (_root, mut fs) = fs("writev-truncate");
        write(&mut fs, "/f", b"longer-than-needed");
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            VfFile::from_path("/f"),
            0,
            b"hi".to_vec(),
        )
        .with_truncate()]))
            .unwrap();
        // O_TRUNC semantics: the stale tail is gone.
        assert_eq!(
            fs.read_raw_impl(&VfFile::from_path("/f"), 0, 100).unwrap(),
            b"hi"
        );

        // A plain overwrite keeps the tail (pwrite semantics).
        write(&mut fs, "/g", b"abcdef");
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            VfFile::from_path("/g"),
            0,
            b"xy".to_vec(),
        )]))
        .unwrap();
        assert_eq!(
            fs.read_raw_impl(&VfFile::from_path("/g"), 0, 100).unwrap(),
            b"xycdef"
        );
    }

    #[test]
    fn eof_is_only_true_at_end() {
        let (_root, mut fs) = fs("eof");
        write(&mut fs, "/f", b"abc");

        let r = fs
            .vread_impl(&[ReadOp::at(VfFile::from_path("/f"), 0, 3)])
            .unwrap();
        assert_eq!(r[0].data, b"abc");
        assert!(!r[0].eof);

        let r = fs
            .vread_impl(&[ReadOp::at(VfFile::from_path("/f"), 0, 4)])
            .unwrap();
        assert_eq!(r[0].data, b"abc");
        assert!(r[0].eof);

        // Zero-length reads never report EOF.
        let r = fs
            .vread_impl(&[ReadOp::at(VfFile::from_path("/f"), 0, 0)])
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
                .err_no(),
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
        assert_eq!(error.err_no(), libc::EOVERFLOW as u32);
        assert_eq!(fs.seek_raw_impl(&fd, 0, SeekFrom::Cur).unwrap(), 1);
        assert_eq!(std::fs::metadata(root.0.join("f")).unwrap().len(), 1);
        assert_eq!(std::fs::read(root.0.join("f")).unwrap(), b"x");

        fs.seek_raw_impl(&fd, i64::MAX, SeekFrom::Set).unwrap();
        let error = fs.seek_raw_impl(&fd, 1, SeekFrom::Cur).unwrap_err();
        assert_eq!(error.err_no(), libc::EOVERFLOW as u32);
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
        assert_eq!(error.err_no(), libc::EOVERFLOW as u32);
        assert!(!fs.exists_impl(Path::new("/overflow")).unwrap());
    }

    // ------------------------------------------------------------------
    // VfFile base/cwd resolution is honored by every path-taking method
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
            fs.lstat_impl(Path::new("/a")).unwrap_err().err_no(),
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
        let abs_src = VfFile::Path {
            base: VfPathBase::Abs,
            path: PathBuf::from("src"),
        };
        let abs_dst = VfFile::Path {
            base: VfPathBase::Abs,
            path: PathBuf::from("dst"),
        };
        fs.vrename_impl(&[(abs_src, abs_dst)]).unwrap();
        assert!(!fs.exists_impl(Path::new("/src")).unwrap());
        assert!(fs.exists_impl(Path::new("/dst")).unwrap());

        // base Cwd resolves against the cwd.
        let cwd_src = VfFile::Path {
            base: VfPathBase::Cwd,
            path: PathBuf::from("src2"),
        };
        let cwd_dst = VfFile::Path {
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
            fs.vrename_with_options_impl(&[], vfsi_core::api::RenameOptions::NoReplace)
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
                    VfFile::from_path("/source"),
                    VfFile::from_path("/source/child/moved"),
                )],
                vfsi_core::api::RenameOptions::NoReplace,
            )
            .unwrap_err();
        assert_eq!(error.err_no(), libc::EINVAL as u32);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn no_replace_empty_vector_is_a_noop() {
        let (_root, mut fs) = fs("rename-noreplace-empty");
        assert!(
            fs.vrename_with_options_impl(&[], vfsi_core::api::RenameOptions::NoReplace)
                .is_ok()
        );
    }

    #[test]
    fn vf_path_rejects_descriptors() {
        let (_root, mut fs) = fs("vf-path");
        write(&mut fs, "/f", b"x");
        let fd = fs.open_raw_impl(Path::new("/f"), 0, 0).unwrap();
        assert_eq!(fs.vf_path(&fd).unwrap_err().err_no(), ERR_INVAL);
        fs.close_impl(&fd).unwrap();
    }

    #[test]
    fn vf_file_cwd_and_cwd_path_resolution() {
        let (_root, mut fs) = fs("cwd-variants");
        fs.mkdir_raw_impl(Path::new("/sub"), 0o755).unwrap();
        write(&mut fs, "/sub/f", b"x");

        // `cwd()` is the cwd itself; `cwd_path` is relative to it.
        assert_eq!(fs.vf_path(&VfFile::cwd()).unwrap(), Path::new(""));
        assert_eq!(
            VfFile::cwd_path("f").path(),
            Some(std::path::Path::new("f"))
        );
        fs.chdir(Path::new("/sub")).unwrap();
        assert_eq!(fs.vf_path(&VfFile::cwd()).unwrap(), Path::new("sub"));
        assert_eq!(
            fs.vf_path(&VfFile::cwd_path("f")).unwrap(),
            Path::new("sub/f")
        );

        // The cwd is a stat target but not a file for read/write.
        let mut a = VfAttrs {
            file: VfFile::cwd(),
            masks: AttrMask::stat(),
            ..VfAttrs::default()
        };
        fs.vgetattrs_impl(std::slice::from_mut(&mut a)).unwrap();
        assert_eq!(a.ftype, VfType::Directory);
        assert_eq!(
            fs.vread_impl(&[ReadOp::new(VfFile::cwd(), VfOffset::At(0), 1)])
                .unwrap_err()
                .err_no(),
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
            VfFile::from_path("/../escape"),
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
            VfFile::from_path("/../sub1/../../sub2"),
            0,
            b"y".to_vec(),
        )
        .with_creation()]))
            .unwrap();
        assert!(fs.exists_impl(Path::new("/sub2")).unwrap());
        assert!(!root.0.parent().unwrap().join("sub2").exists());

        // A lexical "a/../b" path resolves to b.
        write(&mut fs, "/a", b"");
        fs.vrename_impl(&[(VfFile::from_path("/a"), VfFile::from_path("/x/../b"))])
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
            fs.read_raw_impl(&VfFile::from_path("/abs-link"), 0, 6)
                .unwrap(),
            b"inside"
        );
        // ".." components in an absolute target are clamped at the root.
        fs.symlink_raw_impl(Path::new("/sub/../target"), Path::new("/dotdot-link"))
            .unwrap();
        assert_eq!(
            fs.read_raw_impl(&VfFile::from_path("/dotdot-link"), 0, 6)
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
            fs.vread_impl(&[ReadOp::at(VfFile::from_path("/evil"), 0, 8)])
                .unwrap_err()
                .err_no(),
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
            VfFile::from_path("/evil3"),
            0,
            b"x".to_vec(),
        )
        .with_creation()]))
            .unwrap();
        assert_eq!(
            fs.read_raw_impl(&VfFile::from_path("/subdir/created-inside"), 0, 1)
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
                VfFile::from_path("/evil2"),
                0,
                b"x".to_vec()
            )
            .with_creation()]))
                .unwrap_err()
                .err_no(),
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
            VfFile::from_path("/ok-link"),
            0,
            b"z".to_vec(),
        )
        .with_creation()]))
            .unwrap();
        assert_eq!(
            fs.read_raw_impl(&VfFile::from_path("/internal-target"), 0, 1)
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
            fs.vremove_impl(&[VfFile::from_path("/pivot/victim")])
                .unwrap_err(),
            fs.vrename_impl(&[(
                VfFile::from_path("/pivot/victim"),
                VfFile::from_path("/renamed"),
            )])
            .unwrap_err(),
            fs.listdir_impl(Path::new("/pivot"), AttrMask::stat(), 0, false)
                .unwrap_err(),
            fs.vreadlink_impl(&[Path::new("/pivot/link")]).unwrap_err(),
            fs.vhardlink_impl(&[Path::new("/pivot/victim")], &[Path::new("/hard")])
                .unwrap_err(),
            fs.lstat_impl(Path::new("/pivot/victim")).unwrap_err(),
        ] {
            assert!([ERR_ACCES, ERR_NOENT].contains(&error.err_no()));
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
        assert_eq!(error.err_no(), ERR_NOENT);
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
            VfFile::from_path("../x"),
            0,
            b"1".to_vec(),
        )
        .with_creation()]))
            .unwrap();
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            VfFile::from_path("a/../y"),
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
            VfFile::from_path("../z"),
            0,
            b"3".to_vec(),
        )
        .with_creation()]))
            .unwrap();
        assert!(fs.exists_impl(Path::new("/z")).unwrap());
        assert!(!root.0.parent().unwrap().join("z").exists());
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
            file: VfFile::from_path("/f"),
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
            file: VfFile::from_path("/f"),
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
            file: VfFile::from_path("/f"),
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
                    .err_no(),
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
            file: VfFile::from_path("/f"),
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
            file: VfFile::from_path("/f"),
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
    fn lsetattrsv_does_not_follow_symlinks() {
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
            file: VfFile::from_path("/link"),
            masks: AttrMask::MODE,
            mode: 0o600,
            ..VfAttrs::default()
        };
        assert_eq!(
            fs.vsetattrs_raw_nofollow_impl(std::slice::from_ref(&a))
                .unwrap_err()
                .err_no(),
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
            file: VfFile::from_path("/target"),
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
        assert_eq!((e.index(), e.err_no()), (Some(0), ERR_INVAL));
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
            vfsi_core::CopyOption::new().follow_source_symlinks(false),
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
        fs.vcopy_impl(std::slice::from_ref(&pair), CopyOption::new())
            .unwrap();
        assert_eq!(
            fs.file_type_impl(Path::new("/link-dup")).unwrap(),
            VfType::Regular
        );
        assert_eq!(
            fs.read_raw_impl(&VfFile::from_path("/link-dup"), 0, 4)
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
        let options = CopyOption::new().follow_source_symlinks(false);
        let pairs = [
            ExtentPair::new("/source", 2, "/data-copy", 0, Some(4)),
            ExtentPair::new("/dangling", 99, "/dangling-copy", 100, Some(0)),
        ];
        fs.vcopy_impl(&pairs, options).unwrap();
        assert_eq!(
            fs.read_raw_impl(&VfFile::from_path("/data-copy"), 0, 4)
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
        assert_eq!(error.err_no(), libc::EEXIST as u32);
        // Following a dangling link fails, while preserving it succeeds above.
        let error = fs
            .vcopy_impl(
                &[ExtentPair::new("/dangling", 0, "/followed", 0, None)],
                CopyOption::new(),
            )
            .unwrap_err();
        assert_eq!(error.err_no(), libc::ENOENT as u32);
        fs.vcopy_impl(&[], options).unwrap();
    }

    #[test]
    fn shared_tree_copy_preserves_symlinks_and_uses_the_copy_strategy() {
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
            fs.read_raw_impl(&VfFile::from_os_path(&copied_leaf), 0, 4)
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
}
