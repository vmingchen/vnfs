use std::path::{Path, PathBuf};
use vfsi_core::*;
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
                    prop_assert_eq!(error.index_opt(), Some(index));
                    prop_assert_eq!(error.err_no(), libc::EOVERFLOW as u32);
                }
                (expected, actual) => prop_assert!(false, "expected {expected:?}, got {actual:?}"),
            }
        }
    }
    use crate::DummyVecFs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use vfsi_core::RpcError;

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
    fn fs(tag: &str) -> (TempRoot, DummyVecFs) {
        let root = TempRoot::new(tag);
        let fs = DummyVecFs::new(root.0.clone());
        (root, fs)
    }

    #[test]
    fn try_new_reports_setup_errors_instead_of_panicking() {
        let root = TempRoot::new("try-new-error");
        std::fs::create_dir(&root.0).unwrap();
        let not_a_directory = root.0.join("file");
        std::fs::write(&not_a_directory, b"not a directory").unwrap();
        let error = DummyVecFs::try_new(not_a_directory).err().unwrap();
        assert!(matches!(
            error.err_no(),
            value if value == libc::EEXIST as u32 || value == libc::ENOTDIR as u32
        ));
    }

    fn write(fs: &mut DummyVecFs, path: &str, data: &[u8]) {
        fs.writev(&[WriteOp::at(VfFile::from_path(path), 0, data.to_vec()).with_creation()])
            .expect("write");
    }

    // ------------------------------------------------------------------
    // VfError
    // ------------------------------------------------------------------

    #[test]
    fn descriptor_allocator_wraps_and_skips_live_entries() {
        let mut next = i32::MAX;
        let mut files = std::collections::HashMap::from([(1, "live")]);
        let fd = insert_fd(&mut next, &mut files, "new").unwrap();
        assert_eq!(fd, 2);
        assert_eq!(files.get(&1), Some(&"live"));
        assert_eq!(files.get(&2), Some(&"new"));
    }

    #[test]
    fn vf_error_preserves_transport_message() {
        let e = VfError::from_rpc(RpcError::transport("connection refused"), 3);
        assert!(e.is_transport());
        assert_eq!(e.index_opt(), Some(3));
        assert_eq!(e.index_opt(), Some(3));
        assert_eq!(e.err_no(), VF_ERR_RPC);
        assert!(e.to_string().contains("connection refused"));

        // An unattributable transport failure has no op index.
        let e = VfError::from_rpc(RpcError::transport("server gone"), None);
        assert!(e.is_transport());
        assert_eq!(e.index_opt(), None);
        assert!(!e.to_string().contains("op "));

        // Server status errors stay Op errors with the caller-supplied index.
        let e = VfError::from_rpc(RpcError::op(4, ERR_NOENT), 1);
        assert!(!e.is_transport());
        assert_eq!(e.index_opt(), Some(1));
        assert_eq!(e.index_opt(), Some(1));
        assert_eq!(e.err_no(), ERR_NOENT);
    }

    #[test]
    fn vf_error_indexed_and_remap() {
        let e = VfError::from_rpc_indexed(RpcError::op(4, ERR_EXIST));
        assert_eq!((e.index_opt(), e.err_no()), (Some(4), ERR_EXIST));
        assert_eq!(e.index_opt(), Some(4));
        assert_eq!(e.with_index(9).index_opt(), Some(9));
        assert_eq!(
            VfError::transport(2, "boom").with_index(5).index_opt(),
            Some(5)
        );
        assert_eq!(VfError::transport(None, "boom").index_opt(), None);
    }

    // ------------------------------------------------------------------
    // Offsets: no sentinel collision, Cur/End resolution, result offsets
    // ------------------------------------------------------------------

    #[test]
    fn absolute_offset_at_u64_max_minus_one_is_not_cur() {
        let (_root, mut fs) = fs("huge-offset");
        write(&mut fs, "/f", b"abcdefgh");
        let fd = fs.open(Path::new("/f"), 0, 0).unwrap();
        fs.fseek(&fd, 2, SeekFrom::Set).unwrap();

        // Previously u64::MAX - 1 collided with the VF_OFFSET_CUR sentinel and
        // would have read from the current position (2) instead. With the
        // typed offset it is an absolute offset: the platform may reject it
        // (pread beyond i64::MAX) or return an empty read, but never data
        // from the current position.
        match fs.readv(&[ReadOp::new(fd.clone(), VfOffset::At(u64::MAX - 1), 8)]) {
            Err(e) => assert!([ERR_INVAL, libc::EOVERFLOW as u32].contains(&e.err_no())),
            Ok(r) => {
                assert!(r[0].data.is_empty());
                assert!(r[0].eof);
            }
        }
        fs.close(&fd).unwrap();
    }

    #[test]
    fn cur_offset_reads_resolve_and_advance() {
        let (_root, mut fs) = fs("cur");
        write(&mut fs, "/f", b"hello world");
        let fd = fs.open(Path::new("/f"), libc::O_RDWR, 0).unwrap();
        assert_eq!(fs.fseek(&fd, 0, SeekFrom::End).unwrap(), 11);

        let w = fs
            .writev(&[WriteOp::new(fd.clone(), VfOffset::Cur, b"XY".to_vec())])
            .unwrap();
        assert_eq!(w[0].offset, 11); // resolved current position
        let w = fs
            .writev(&[WriteOp::new(fd.clone(), VfOffset::Cur, b"Z".to_vec())])
            .unwrap();
        assert_eq!(w[0].offset, 13);

        let r = fs
            .readv(&[ReadOp::new(fd.clone(), VfOffset::Cur, 100)])
            .unwrap();
        assert_eq!(r[0].offset, 14); // resolved, not the Cur sentinel
        assert!(r[0].data.is_empty());
        assert!(r[0].eof);
        fs.close(&fd).unwrap();
    }

    #[test]
    fn end_offset_writes_append_and_reads_at_end() {
        let (_root, mut fs) = fs("end");
        write(&mut fs, "/f", b"hello world");

        let fd = fs.open(Path::new("/f"), libc::O_RDWR, 0).unwrap();
        let w = fs
            .writev(&[WriteOp::new(fd.clone(), VfOffset::End, b"!".to_vec())])
            .unwrap();
        assert_eq!(w[0].offset, 11);
        fs.close(&fd).unwrap();

        // A read positioned at End starts at the file size, so it is at EOF.
        let r = fs
            .readv(&[ReadOp::new(VfFile::from_path("/f"), VfOffset::End, 5)])
            .unwrap();
        assert_eq!(r[0].offset, 12); // the resolved start is the file size
        assert!(r[0].data.is_empty());
        assert!(r[0].eof);

        let r = fs
            .readv(&[ReadOp::new(VfFile::from_path("/f"), VfOffset::End, 100)])
            .unwrap();
        assert!(r[0].eof);
        assert_eq!(fs.stat(Path::new("/f")).unwrap().size, 12);
    }

    #[test]
    fn writev_truncate_removes_stale_tail() {
        let (_root, mut fs) = fs("writev-truncate");
        write(&mut fs, "/f", b"longer-than-needed");
        fs.writev(&[WriteOp::at(VfFile::from_path("/f"), 0, b"hi".to_vec()).with_truncate()])
            .unwrap();
        // O_TRUNC semantics: the stale tail is gone.
        assert_eq!(fs.read(&VfFile::from_path("/f"), 0, 100).unwrap(), b"hi");

        // A plain overwrite keeps the tail (pwrite semantics).
        write(&mut fs, "/g", b"abcdef");
        fs.writev(&[WriteOp::at(VfFile::from_path("/g"), 0, b"xy".to_vec())])
            .unwrap();
        assert_eq!(
            fs.read(&VfFile::from_path("/g"), 0, 100).unwrap(),
            b"xycdef"
        );
    }

    #[test]
    fn eof_is_only_true_at_end() {
        let (_root, mut fs) = fs("eof");
        write(&mut fs, "/f", b"abc");

        let r = fs
            .readv(&[ReadOp::at(VfFile::from_path("/f"), 0, 3)])
            .unwrap();
        assert_eq!(r[0].data, b"abc");
        assert!(!r[0].eof);

        let r = fs
            .readv(&[ReadOp::at(VfFile::from_path("/f"), 0, 4)])
            .unwrap();
        assert_eq!(r[0].data, b"abc");
        assert!(r[0].eof);

        // Zero-length reads never report EOF.
        let r = fs
            .readv(&[ReadOp::at(VfFile::from_path("/f"), 0, 0)])
            .unwrap();
        assert!(r[0].data.is_empty());
        assert!(!r[0].eof);
    }

    #[test]
    fn fseek_takes_shared_ref_and_works() {
        let (_root, mut fs) = fs("fseek");
        write(&mut fs, "/f", b"hello world");
        let fd = fs.open(Path::new("/f"), 0, 0).unwrap();

        assert_eq!(fs.fseek(&fd, 6, SeekFrom::Set).unwrap(), 6);
        let r = fs
            .readv(&[ReadOp::new(fd.clone(), VfOffset::Cur, 5)])
            .unwrap();
        assert_eq!(r[0].data, b"world");

        assert_eq!(fs.fseek(&fd, -5, SeekFrom::End).unwrap(), 6);
        assert_eq!(fs.fseek(&fd, 0, SeekFrom::Cur).unwrap(), 6);
        assert_eq!(
            fs.fseek(&fd, -100, SeekFrom::Set).unwrap_err().err_no(),
            ERR_INVAL
        );
        fs.close(&fd).unwrap();
    }

    #[test]
    fn offset_overflow_is_reported_without_io_or_cursor_wraparound() {
        let (_root, mut fs) = fs("offset-overflow");
        write(&mut fs, "/f", b"x");
        let fd = fs.open(Path::new("/f"), libc::O_RDWR, 0).unwrap();

        let error = fs
            .writev(&[WriteOp::at(fd.clone(), u64::MAX, b"xx".to_vec())])
            .unwrap_err();
        assert_eq!(error.err_no(), libc::EOVERFLOW as u32);

        fs.fseek(&fd, i64::MAX, SeekFrom::Set).unwrap();
        let error = fs.fseek(&fd, 1, SeekFrom::Cur).unwrap_err();
        assert_eq!(error.err_no(), libc::EOVERFLOW as u32);
        fs.close(&fd).unwrap();
    }

    #[test]
    fn adb_layout_overflow_fails_before_creating_the_file() {
        let (_root, mut fs) = fs("adb-overflow");
        let pattern = Adb::blocknum_only("/overflow", u64::MAX, 2, 2, 0, 0);
        let error = fs.write_adb(&[pattern]).unwrap_err();
        assert_eq!(error.err_no(), libc::EOVERFLOW as u32);
        assert!(!fs.exists(Path::new("/overflow")).unwrap());
    }

    // ------------------------------------------------------------------
    // VfFile base/cwd resolution is honored by every path-taking method
    // ------------------------------------------------------------------

    #[test]
    fn cwd_relative_unlink_targets_cwd() {
        let (_root, mut fs) = fs("cwd-unlink");
        fs.mkdir(Path::new("/sub"), 0o755).unwrap();
        fs.chdir(Path::new("sub")).unwrap();

        write(&mut fs, "a", b"x"); // cwd-relative write
        fs.unlink(Path::new("a")).unwrap();

        // The file was removed from sub/, not from the root.
        assert!(!fs.exists(Path::new("a")).unwrap());
        assert_eq!(fs.lstat(Path::new("/a")).unwrap_err().err_no(), ERR_NOENT);
    }

    #[test]
    fn renamev_honors_path_base() {
        let (_root, mut fs) = fs("rename-base");
        fs.mkdir(Path::new("/sub"), 0o755).unwrap();
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
        fs.renamev(&[(abs_src, abs_dst)]).unwrap();
        assert!(!fs.exists(Path::new("/src")).unwrap());
        assert!(fs.exists(Path::new("/dst")).unwrap());

        // base Cwd resolves against the cwd.
        let cwd_src = VfFile::Path {
            base: VfPathBase::Cwd,
            path: PathBuf::from("src2"),
        };
        let cwd_dst = VfFile::Path {
            base: VfPathBase::Cwd,
            path: PathBuf::from("dst2"),
        };
        fs.renamev(&[(cwd_src, cwd_dst)]).unwrap();
        assert!(!fs.exists(Path::new("/sub/src2")).unwrap());
        assert!(fs.exists(Path::new("/sub/dst2")).unwrap());
    }

    #[test]
    fn vf_path_rejects_descriptors() {
        let (_root, mut fs) = fs("vf-path");
        write(&mut fs, "/f", b"x");
        let fd = fs.open(Path::new("/f"), 0, 0).unwrap();
        assert_eq!(fs.vf_path(&fd).unwrap_err().err_no(), ERR_INVAL);
        fs.close(&fd).unwrap();
    }

    #[test]
    fn vf_file_cwd_and_cwd_path_resolution() {
        let (_root, mut fs) = fs("cwd-variants");
        fs.mkdir(Path::new("/sub"), 0o755).unwrap();
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
        fs.getattrsv(std::slice::from_mut(&mut a)).unwrap();
        assert_eq!(a.ftype, VfType::Directory);
        assert_eq!(
            fs.readv(&[ReadOp::new(VfFile::cwd(), VfOffset::At(0), 1)])
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
        fs.writev(&[
            WriteOp::at(VfFile::from_path("/../escape"), 0, b"x".to_vec()).with_creation(),
        ])
        .unwrap();
        assert!(fs.exists(Path::new("/escape")).unwrap());
        assert!(!root.0.parent().unwrap().join("escape").exists());

        // "/.." and "/../../x" stay under the root.
        let st = fs.stat(Path::new("/..")).unwrap();
        assert_eq!(st.ftype, VfType::Directory);
        fs.writev(&[
            WriteOp::at(VfFile::from_path("/../sub1/../../sub2"), 0, b"y".to_vec()).with_creation(),
        ])
        .unwrap();
        assert!(fs.exists(Path::new("/sub2")).unwrap());
        assert!(!root.0.parent().unwrap().join("sub2").exists());

        // A lexical "a/../b" path resolves to b.
        write(&mut fs, "/a", b"");
        fs.renamev(&[(VfFile::from_path("/a"), VfFile::from_path("/x/../b"))])
            .unwrap();
        assert!(fs.exists(Path::new("/b")).unwrap());
        assert!(!fs.exists(Path::new("/x")).unwrap());
    }

    #[test]
    fn dummy_root_resolves_absolute_symlink_targets_inside_root() {
        let (root, mut fs) = fs("sandbox-symlink");
        write(&mut fs, "/target", b"inside");

        // An absolute target is chroot-relative: "/target" is the root's
        // "target", so reads through the link work.
        fs.symlink(Path::new("/target"), Path::new("/abs-link"))
            .unwrap();
        assert_eq!(
            fs.read(&VfFile::from_path("/abs-link"), 0, 6).unwrap(),
            b"inside"
        );
        // ".." components in an absolute target are clamped at the root.
        fs.symlink(Path::new("/sub/../target"), Path::new("/dotdot-link"))
            .unwrap();
        assert_eq!(
            fs.read(&VfFile::from_path("/dotdot-link"), 0, 6).unwrap(),
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
        fs.symlink(Path::new(&outside), Path::new("/evil")).unwrap();
        assert_eq!(
            fs.readv(&[ReadOp::at(VfFile::from_path("/evil"), 0, 8)])
                .unwrap_err()
                .err_no(),
            ERR_NOENT,
            "resolves inside the root where nothing exists yet"
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");

        // Creating through a chroot-relative absolute target lands inside the
        // root.
        fs.mkdir(Path::new("/subdir"), 0o755).unwrap();
        fs.symlink(Path::new("/subdir/created-inside"), Path::new("/evil3"))
            .unwrap();
        fs.writev(&[WriteOp::at(VfFile::from_path("/evil3"), 0, b"x".to_vec()).with_creation()])
            .unwrap();
        assert_eq!(
            fs.read(&VfFile::from_path("/subdir/created-inside"), 0, 1)
                .unwrap(),
            b"x"
        );

        // The link itself can still be inspected and removed (no-follow).
        assert_eq!(
            fs.lstat(Path::new("/abs-link")).unwrap().ftype,
            VfType::Symlink
        );
        fs.readlink(Path::new("/abs-link")).unwrap();
        fs.unlink(Path::new("/abs-link")).unwrap();

        // A dangling symlink to an absolute path still cannot touch the
        // outside of the root when creating through it.
        let dangling = root.0.parent().unwrap().join("never-created");
        fs.symlink(Path::new(&dangling), Path::new("/evil2"))
            .unwrap();
        assert_eq!(
            fs.writev(
                &[WriteOp::at(VfFile::from_path("/evil2"), 0, b"x".to_vec()).with_creation()]
            )
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
        fs.symlink(Path::new("internal-target"), Path::new("/ok-link"))
            .unwrap();
        fs.writev(&[WriteOp::at(VfFile::from_path("/ok-link"), 0, b"z".to_vec()).with_creation()])
            .unwrap();
        assert_eq!(
            fs.read(&VfFile::from_path("/internal-target"), 0, 1)
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
            fs.removev(&[VfFile::from_path("/pivot/victim")])
                .unwrap_err(),
            fs.renamev(&[(
                VfFile::from_path("/pivot/victim"),
                VfFile::from_path("/renamed"),
            )])
            .unwrap_err(),
            fs.listdir(Path::new("/pivot"), AttrMask::stat(), 0, false)
                .unwrap_err(),
            fs.readlinkv(&[Path::new("/pivot/link")]).unwrap_err(),
            fs.hardlinkv(&[Path::new("/pivot/victim")], &[Path::new("/hard")])
                .unwrap_err(),
            fs.lstat(Path::new("/pivot/victim")).unwrap_err(),
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
        DummyVecFs::protect_create_open(&anchored, &mut options);
        options.open(&anchored).unwrap();

        assert!(root.0.join("moved/new").exists());
        assert_eq!(std::fs::read(outside.0.join("new")).unwrap(), b"outside");
    }

    #[test]
    fn dummy_open_by_path_abs_is_root_relative() {
        let (_root, mut fs) = fs("open-abs");
        let fd = fs
            .open_by_path(
                VfPathBase::Abs,
                Path::new("rel"),
                libc::O_CREAT | libc::O_RDWR,
                0o644,
            )
            .unwrap();
        fs.writev(&[WriteOp::new(fd.clone(), VfOffset::At(0), b"x".to_vec())])
            .unwrap();
        fs.close(&fd).unwrap();
        assert!(fs.exists(Path::new("/rel")).unwrap());
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

        assert_eq!(fs.stat(Path::new("/fifo")).unwrap().ftype, VfType::Fifo);
        assert_eq!(fs.lstat(Path::new("/fifo")).unwrap().ftype, VfType::Fifo);
        assert_eq!(fs.stat(Path::new("/sock")).unwrap().ftype, VfType::Socket);
        let listed = fs
            .listdir(Path::new("/"), AttrMask::default(), 0, false)
            .unwrap();
        assert!(listed.iter().any(|e| e.ftype == VfType::Fifo));
        assert!(listed.iter().any(|e| e.ftype == VfType::Socket));

        unsafe { libc::close(fd) };
    }

    #[test]
    fn dummy_descriptor_sees_external_truncation() {
        let (root, mut fs) = fs("ext-trunc");
        write(&mut fs, "/f", b"0123456789");
        let fd = fs.open(Path::new("/f"), libc::O_RDWR, 0).unwrap();
        let real = root.0.join("f");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&real)
            .unwrap()
            .set_len(3)
            .unwrap();
        let r = fs
            .readv(&[ReadOp::new(fd.clone(), VfOffset::At(0), 10)])
            .unwrap();
        assert_eq!(r[0].data, b"012", "descriptor sees the new size");
        fs.close(&fd).unwrap();
    }

    #[test]
    fn dummy_cwd_dotdot_stays_in_root() {
        let (root, mut fs) = fs("cwd-dotdot");
        fs.mkdir(Path::new("/a"), 0o755).unwrap();
        fs.chdir(Path::new("/a")).unwrap();

        fs.writev(&[WriteOp::at(VfFile::from_path("../x"), 0, b"1".to_vec()).with_creation()])
            .unwrap();
        fs.writev(&[WriteOp::at(VfFile::from_path("a/../y"), 0, b"2".to_vec()).with_creation()])
            .unwrap();
        assert!(fs.exists(Path::new("/x")).unwrap());
        // From cwd /a, "a/../y" resolves to /a/y (the ".." cancels the "a").
        assert!(fs.exists(Path::new("/a/y")).unwrap());
        assert!(!fs.exists(Path::new("/y")).unwrap());
        assert!(!root.0.parent().unwrap().join("x").exists());
        assert!(!root.0.parent().unwrap().join("y").exists());

        // ".." from the root clamps at the root instead of escaping.
        fs.chdir(Path::new("/")).unwrap();
        fs.writev(&[WriteOp::at(VfFile::from_path("../z"), 0, b"3".to_vec()).with_creation()])
            .unwrap();
        assert!(fs.exists(Path::new("/z")).unwrap());
        assert!(!root.0.parent().unwrap().join("z").exists());
    }

    #[test]
    fn dummy_named_attr_detection() {
        use std::ffi::CString;
        let (root, mut fs) = fs("xattr");
        write(&mut fs, "/f", b"x");
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
        fs.getattrsv(std::slice::from_mut(&mut a)).unwrap();
        assert!(a.has_named_attr);
        assert!(a.returned.contains(AttrMask::NAMED_ATTR));
    }

    // ------------------------------------------------------------------
    // Attributes: returned tracking, strict setattrsv, lsetattrsv
    // ------------------------------------------------------------------

    #[test]
    fn getattrsv_reports_returned_mask() {
        let (_root, mut fs) = fs("returned");
        write(&mut fs, "/f", b"x");

        let a = fs.stat(Path::new("/f")).unwrap();
        assert_eq!(a.returned, AttrMask::stat());
        assert!(a.returned.contains(AttrMask::MODE));

        let mut a = VfAttrs {
            file: VfFile::from_path("/f"),
            masks: AttrMask::MODE | AttrMask::SIZE | AttrMask::MTIME,
            ..VfAttrs::default()
        };
        fs.getattrsv(std::slice::from_mut(&mut a)).unwrap();
        assert_eq!(
            a.returned,
            AttrMask::MODE | AttrMask::SIZE | AttrMask::MTIME
        );
    }

    #[test]
    fn setattrsv_rejects_unsupported_bits() {
        let (_root, mut fs) = fs("setattr-strict");
        write(&mut fs, "/f", b"x");

        let mut a = VfAttrs {
            file: VfFile::from_path("/f"),
            masks: AttrMask::BLOCKS,
            ..VfAttrs::default()
        };
        assert_eq!(
            fs.setattrsv(std::slice::from_ref(&a)).unwrap_err().err_no(),
            VF_ERR_UNSUPPORTED
        );

        a.masks = AttrMask::MODE | AttrMask::BLOCKS;
        assert_eq!(
            fs.setattrsv(std::slice::from_ref(&a)).unwrap_err().err_no(),
            VF_ERR_UNSUPPORTED
        );

        // MODE-only still works.
        a.masks = AttrMask::MODE;
        a.mode = 0o640;
        fs.setattrsv(std::slice::from_ref(&a)).unwrap();
        assert_eq!(fs.lstat(Path::new("/f")).unwrap().mode & 0o7777, 0o640);
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
        fs.setattrsv(std::slice::from_ref(&update)).unwrap();

        let mut actual = VfAttrs {
            file: VfFile::from_path("/f"),
            masks: AttrMask::ATIME | AttrMask::MTIME | AttrMask::SIZE,
            ..VfAttrs::default()
        };
        fs.getattrsv(std::slice::from_mut(&mut actual)).unwrap();
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
        let (_root, mut fs) = fs("lsetattr");
        write(&mut fs, "/target", b"x");
        fs.symlink(Path::new("/target"), Path::new("/link"))
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
            fs.lsetattrsv(std::slice::from_ref(&a))
                .unwrap_err()
                .err_no(),
            VF_ERR_UNSUPPORTED
        );

        // Regular files are set normally.
        let a = VfAttrs {
            file: VfFile::from_path("/target"),
            masks: AttrMask::MODE,
            mode: 0o600,
            ..VfAttrs::default()
        };
        fs.lsetattrsv(std::slice::from_ref(&a)).unwrap();
        assert_eq!(fs.lstat(Path::new("/target")).unwrap().mode & 0o7777, 0o600);
    }

    // ------------------------------------------------------------------
    // exists / file_type use lstat semantics
    // ------------------------------------------------------------------

    #[test]
    fn exists_and_file_type_use_lstat_semantics() {
        let (_root, mut fs) = fs("lstat");
        write(&mut fs, "/f", b"x");
        fs.symlink(Path::new("missing-target"), Path::new("/dangling"))
            .unwrap();

        assert!(fs.exists(Path::new("/dangling")).unwrap());
        assert_eq!(
            fs.file_type(Path::new("/dangling")).unwrap(),
            VfType::Symlink
        );
        assert_eq!(fs.file_type(Path::new("/f")).unwrap(), VfType::Regular);
    }

    // ------------------------------------------------------------------
    // openv length contract, listdir limits, walk via dyn VecFs
    // ------------------------------------------------------------------

    #[test]
    fn openv_rejects_mismatched_lengths() {
        let (_root, mut fs) = fs("openv");
        use libc::O_CREAT;
        let e = VecFs::openv(
            &mut fs,
            &[Path::new("/a"), Path::new("/b")],
            &[O_CREAT],
            &[0o644],
        )
        .unwrap_err();
        assert_eq!((e.index_opt(), e.err_no()), (Some(0), ERR_INVAL));
    }

    #[test]
    fn listdir_zero_max_count_is_unlimited() {
        let (_root, mut fs) = fs("listdir");
        fs.mkdir(Path::new("/d"), 0o755).unwrap();
        write(&mut fs, "/d/a", b"1");
        write(&mut fs, "/d/b", b"2");

        let all = fs
            .listdir(Path::new("/d"), AttrMask::default(), 0, false)
            .unwrap();
        assert_eq!(all.len(), 2);
        let one = fs
            .listdir(Path::new("/d"), AttrMask::default(), 1, false)
            .unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn walk_works_through_dyn_vecfs() {
        let (_root, mut fs) = fs("walk-dyn");
        fs.mkdir(Path::new("/sub"), 0o755).unwrap();
        write(&mut fs, "/sub/a", b"1");

        let mut dyn_fs: Box<dyn VecFs> = Box::new(fs);
        let mut visited: Vec<String> = Vec::new();
        let entries = dyn_fs
            .walk(Path::new(""), AttrMask::stat(), &mut |dir, _| {
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
    fn lcopyv_copies_symlinks_as_symlinks() {
        let (_root, mut fs) = fs("lcopyv");
        write(&mut fs, "/target", b"data");
        fs.symlink(Path::new("target"), Path::new("/link")).unwrap();

        let pair = ExtentPair::new("/link", 0, "/link-copy", 0, None);
        fs.lcopyv(std::slice::from_ref(&pair)).unwrap();
        assert_eq!(
            fs.file_type(Path::new("/link-copy")).unwrap(),
            VfType::Symlink
        );
        assert_eq!(
            fs.readlink(Path::new("/link-copy")).unwrap(),
            fs.readlink(Path::new("/link")).unwrap()
        );

        // dupv copies the target's data instead.
        let pair = ExtentPair::new("/link", 0, "/link-dup", 0, None);
        fs.dupv(std::slice::from_ref(&pair)).unwrap();
        assert_eq!(
            fs.file_type(Path::new("/link-dup")).unwrap(),
            VfType::Regular
        );
        assert_eq!(
            fs.read(&VfFile::from_path("/link-dup"), 0, 4).unwrap(),
            b"data"
        );
    }

    #[test]
    fn deep_recursive_operations_use_bounded_call_stack() {
        const DEPTH: usize = 384;
        let (_root, mut fs) = fs("deep-iterative");
        fs.mkdir(Path::new("/source"), 0o755).unwrap();
        let mut directory = PathBuf::from("/source");
        for _ in 0..DEPTH {
            directory.push("d");
            fs.mkdir(&directory, 0o755).unwrap();
        }
        let leaf = directory.join("leaf");
        fs.writev(&[
            WriteOp::from_os_path(&leaf, VfOffset::At(0), b"deep".to_vec()).with_creation(),
        ])
        .unwrap();

        let listed = fs
            .listdir(Path::new("/source"), AttrMask::MODE, 0, true)
            .unwrap();
        assert_eq!(listed.len(), DEPTH + 1);
        fs.cp_recursive(Path::new("/source"), Path::new("/copy"), false, false)
            .unwrap();
        let copied_leaf = Path::new("/copy").join(
            leaf.strip_prefix("/source")
                .expect("leaf remains below source"),
        );
        assert_eq!(
            fs.read(&VfFile::from_os_path(&copied_leaf), 0, 4).unwrap(),
            b"deep"
        );
        fs.rm(&[Path::new("/source"), Path::new("/copy")], true)
            .unwrap();
        assert!(!fs.exists(Path::new("/source")).unwrap());
        assert!(!fs.exists(Path::new("/copy")).unwrap());
    }
}
