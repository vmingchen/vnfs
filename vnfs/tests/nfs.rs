//! Tests for the `tc` module: one test per `tc_api.h` call, run against the
//! local NFSv4.1 server (127.0.0.1, export `/`). Requires root to read/write
//! the export, so run with `sudo` and the libntirpc library path:
//!
//! ```sh
//! sudo LD_LIBRARY_PATH=$PWD/target/debug/build/libntirpc-sys-*/out/ntirpc/install/lib \
//!     cargo test -p vnfs --test tc_api -- --test-threads=1
//! ```

use nfsv41_sys::nfsstat4_NFS4ERR_EXIST;
use vnfs::NfsVecFs;
use vnfs::nfs::*;

mod common;

#[test]
fn shared_suite_on_nfs() {
    // The same assertions that run against the std::fs DummyVecFs must pass
    // on the NFS backend.
    let mut c = client();
    let dir = format!("/tcore_{}", std::process::id());
    common::run_suite(&mut c, &dir);
}

fn client() -> NfsVecFs {
    NfsVecFs::connect("127.0.0.1").expect("connect to local nfs server")
}

/// Unique working directory for a test, created on demand. Each test gets its
/// own top-level directory (no shared intermediate dir), so parallel test
/// threads never race to create the same parent.
fn workdir(name: &str) -> String {
    format!("/t{}_{}", name, std::process::id())
}

/// Make sure the work directory exists (create ancestors, then itself).
fn setup_dir(name: &str) -> String {
    let dir = workdir(name);
    let c = client();
    let mut c = c;
    c.ensure_dir(&dir, 0o755).expect("ensure_dir workdir");
    dir
}

// ---------------------------------------------------------------------------
// init / deinit
// ---------------------------------------------------------------------------

#[test]
fn init_deinit() {
    // tc_init / tc_deinit: connect must succeed and drop must not panic; the
    // session cleanup (DESTROY_SESSION + DESTROY_CLIENTID) runs in Drop.
    let c = client();
    let cwd = c.getcwd();
    assert_eq!(cwd, "/");
    drop(c);
}

// ---------------------------------------------------------------------------
// open / close
// ---------------------------------------------------------------------------

#[test]
fn open_by_path_and_close() {
    let dir = setup_dir("open_close");
    let f = format!("{}/f.txt", dir);
    let mut c = client();
    let tf = c
        .open(&f, libc::O_CREAT | libc::O_RDWR, 0o644)
        .expect("open/create");
    assert!(tf.is_descriptor());
    c.close(&tf).expect("close");
}

#[test]
fn open_excl_fails_if_exists() {
    let dir = setup_dir("open_excl");
    let f = format!("{}/exists.txt", dir);
    let mut c = client();
    let first = c.open(&f, libc::O_CREAT | libc::O_WRONLY, 0o644).unwrap();
    c.close(&first).unwrap();
    let r = c.open(&f, libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY, 0o644);
    // O_EXCL on an existing file must fail with NFS4ERR_EXIST (17), surfaced
    // directly by the structured error (no string parsing).
    match r {
        Err(e) => assert_eq!(e.err_no, nfsstat4_NFS4ERR_EXIST, "got {:?}", e),
        Ok(_) => panic!("O_EXCL on existing file must fail"),
    }
}

#[test]
fn openv_closev() {
    let dir = setup_dir("openv");
    let paths = [
        format!("{}/a.txt", dir),
        format!("{}/b.txt", dir),
        format!("{}/c.txt", dir),
    ];
    let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    let mut c = client();
    let files = c
        .openv_simple(&refs, libc::O_CREAT | libc::O_RDWR, 0o644)
        .expect("openv_simple");
    assert_eq!(files.len(), 3);
    c.closev(&files).expect("closev");
}

// ---------------------------------------------------------------------------
// chdir / getcwd
// ---------------------------------------------------------------------------

#[test]
fn chdir_getcwd() {
    let dir = setup_dir("chdir");
    let mut c = client();
    assert_eq!(c.getcwd(), "/");
    c.chdir(&dir).expect("chdir");
    assert_eq!(c.getcwd(), dir);

    // Relative resolution now happens against the new cwd.
    c.mkdir("rel", 0o755).expect("mkdir relative");
    let st = c.stat("/").expect("stat export root via absolute path");
    assert_eq!(st.ftype, VfType::Directory, "root is a directory");
}

// ---------------------------------------------------------------------------
// readv / writev
// ---------------------------------------------------------------------------

#[test]
fn writev_then_readv() {
    let dir = setup_dir("rwv");
    let f = format!("{}/data.bin", dir);
    let mut c = client();

    let payload = b"the quick brown fox jumps over the lazy dog\n".to_vec();
    let mut w = VfIoVec::from_path(&f, 0, payload.len(), payload.clone());
    w.is_creation = true;
    c.writev(std::slice::from_mut(&mut w)).expect("writev");
    assert_eq!(w.length, payload.len(), "all bytes written");

    let mut r = VfIoVec::from_path(&f, 0, payload.len(), Vec::new());
    c.readv(std::slice::from_mut(&mut r)).expect("readv");
    assert_eq!(r.data, payload, "read back what was written");
    assert!(!r.is_failure);
}

#[test]
fn readv_eof() {
    let dir = setup_dir("readv_eof");
    let f = format!("{}/short.txt", dir);
    let mut c = client();
    let payload = b"abcdefghij".to_vec();
    let mut w = VfIoVec::from_path(&f, 0, payload.len(), payload.clone());
    w.is_creation = true;
    c.writev(std::slice::from_mut(&mut w)).unwrap();

    // Ask for more than exists: must not fail and must hit EOF.
    let mut r = VfIoVec::from_path(&f, 0, 100, Vec::new());
    c.readv(std::slice::from_mut(&mut r)).expect("readv");
    assert_eq!(r.length, payload.len());
    assert!(r.is_eof, "short read must set eof");
}

#[test]
fn readv_multiple_files() {
    let dir = setup_dir("readv_multi");
    let mut c = client();
    let mut iovs = Vec::new();
    for (i, name) in ["x", "y", "z"].iter().enumerate() {
        let f = format!("{}/{}.txt", dir, name);
        let payload = vec![b'a' + i as u8; 8];
        let mut w = VfIoVec::from_path(&f, 0, payload.len(), payload);
        w.is_creation = true;
        c.writev(std::slice::from_mut(&mut w)).unwrap();
        let mut r = VfIoVec::from_path(&f, 0, 8, Vec::new());
        c.readv(std::slice::from_mut(&mut r)).unwrap();
        iovs.push(r);
    }
    assert_eq!(iovs[0].data, vec![b'a'; 8]);
    assert_eq!(iovs[1].data, vec![b'b'; 8]);
    assert_eq!(iovs[2].data, vec![b'c'; 8]);
}

// ---------------------------------------------------------------------------
// getattrs / stat / lstat / fstat / exists
// ---------------------------------------------------------------------------

fn make_file(path: &str, content: &[u8]) -> NfsVecFs {
    let mut c = client();
    let mut w = VfIoVec::from_path(path, 0, content.len(), content.to_vec());
    w.is_creation = true;
    c.writev(std::slice::from_mut(&mut w)).unwrap();
    c
}

#[test]
fn stat_and_lstat() {
    let dir = setup_dir("stat");
    let f = format!("{}/s.txt", dir);
    let content = b"1234567890".to_vec();
    let mut c = make_file(&f, &content);

    let st = c.stat(&f).expect("stat");
    assert_eq!(st.size, content.len() as u64);
    assert!(st.nlink >= 1);
    assert!(st.fileid != 0);

    let lst = c.lstat(&f).expect("lstat");
    assert_eq!(lst.fileid, st.fileid);
}

#[test]
fn fstat() {
    let dir = setup_dir("fstat");
    let f = format!("{}/fs.txt", dir);
    let content = b"fstat me".to_vec();
    let mut c = make_file(&f, &content);

    let tf = c.open(&f, libc::O_RDONLY, 0).expect("open");
    let st = c.fstat(&tf).expect("fstat");
    assert_eq!(st.size, content.len() as u64);
    c.close(&tf).unwrap();
}

#[test]
fn exists() {
    let dir = setup_dir("exists");
    let f = format!("{}/e.txt", dir);
    let mut c = make_file(&f, b"hi");
    assert!(c.exists(&f));
    assert!(!c.exists(&format!("{}/missing.txt", dir)));
}

#[test]
fn getattrsv() {
    let dir = setup_dir("getattrsv");
    let mut c = client();
    let mut attrs = Vec::new();
    for name in ["g0", "g1", "g2"] {
        let f = format!("{}/{}.txt", dir, name);
        let mut w = VfIoVec::from_path(&f, 0, 3, vec![b'x'; 3]);
        w.is_creation = true;
        c.writev(std::slice::from_mut(&mut w)).unwrap();
        attrs.push(VfAttrs {
            file: VfFile::from_path(&f),
            masks: AttrMask {
                has_mode: true,
                has_size: true,
                has_nlink: true,
                has_fileid: true,
                ..AttrMask::default()
            },
            ..VfAttrs::default()
        });
    }
    c.getattrsv(&mut attrs).expect("getattrsv");
    for a in &attrs {
        assert_eq!(a.size, 3);
        assert!(a.fileid != 0);
    }
}

// ---------------------------------------------------------------------------
// setattrs / lsetattrs
// ---------------------------------------------------------------------------

#[test]
fn setattrsv_mode() {
    let dir = setup_dir("setattrs");
    let f = format!("{}/perm.txt", dir);
    let mut c = make_file(&f, b"data");
    c.setattrsv(&[VfAttrs {
        file: VfFile::from_path(&f),
        masks: AttrMask {
            has_mode: true,
            ..AttrMask::default()
        },
        mode: 0o600,
        ..VfAttrs::default()
    }])
    .expect("setattrsv");
    let st = c.stat(&f).expect("stat");
    assert_eq!(st.mode & 0o777, 0o600);
}

#[test]
fn setattrsv_size_truncate() {
    let dir = setup_dir("setattrs_size");
    let f = format!("{}/trunc.txt", dir);
    let content = b"abcdefghijklmnop".to_vec();
    let mut c = make_file(&f, &content);
    c.setattrsv(&[VfAttrs {
        file: VfFile::from_path(&f),
        masks: AttrMask {
            has_size: true,
            ..AttrMask::default()
        },
        size: 5,
        ..VfAttrs::default()
    }])
    .expect("setattrsv");
    let st = c.stat(&f).expect("stat");
    assert_eq!(st.size, 5);
}

#[test]
fn lsetattrsv() {
    let dir = setup_dir("lsetattrs");
    let f = format!("{}/l.txt", dir);
    let mut c = make_file(&f, b"data");
    c.lsetattrsv(&[VfAttrs {
        file: VfFile::from_path(&f),
        masks: AttrMask {
            has_mode: true,
            ..AttrMask::default()
        },
        mode: 0o640,
        ..VfAttrs::default()
    }])
    .expect("lsetattrsv");
    assert_eq!(c.stat(&f).unwrap().mode & 0o777, 0o640);
}

// ---------------------------------------------------------------------------
// listdir
// ---------------------------------------------------------------------------

#[test]
fn listdir() {
    let dir = setup_dir("listdir");
    let mut c = client();
    c.ensure_dir(&format!("{}/sub", dir), 0o755).unwrap();
    for (i, name) in ["a.txt", "b.txt", "c.txt"].iter().enumerate() {
        let f = format!("{}/{}", dir, name);
        let mut w = VfIoVec::from_path(&f, 0, 4, vec![b'a' + i as u8; 4]);
        w.is_creation = true;
        c.writev(std::slice::from_mut(&mut w)).unwrap();
    }
    let contents = c
        .listdir(&dir, AttrMask::default(), 0, false)
        .expect("listdir");
    let names: Vec<&str> = contents
        .iter()
        .map(|a| a.file.path().unwrap().to_str().unwrap())
        .collect();
    assert!(names.iter().any(|n| n.ends_with("a.txt")));
    assert!(names.iter().any(|n| n.ends_with("sub")));
}

#[test]
fn listdir_recursive() {
    let dir = setup_dir("listdir_rec");
    let mut c = client();
    c.ensure_dir(&format!("{}/d1/d2", dir), 0o755).unwrap();
    for f in [format!("{}/top.txt", dir), format!("{}/d1/deep.txt", dir)] {
        let mut w = VfIoVec::from_path(&f, 0, 2, b"ok".to_vec());
        w.is_creation = true;
        c.writev(std::slice::from_mut(&mut w)).unwrap();
    }
    let contents = c
        .listdir(&dir, AttrMask::default(), 0, true)
        .expect("listdir recursive");
    let joined: Vec<String> = contents
        .iter()
        .map(|a| a.file.path().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(
        joined.iter().any(|p| p.ends_with("deep.txt")),
        "recursive listing finds deep.txt: {:?}",
        joined
    );
    assert!(joined.iter().any(|p| p.ends_with("top.txt")));
}

// ---------------------------------------------------------------------------
// rename
// ---------------------------------------------------------------------------

#[test]
fn renamev() {
    let dir = setup_dir("rename");
    let f = format!("{}/old.txt", dir);
    let g = format!("{}/new.txt", dir);
    let mut c = make_file(&f, b"rename me");
    let pairs = [(VfFile::from_path(&f), VfFile::from_path(&g))];
    c.renamev(&pairs).expect("renamev");
    assert!(!c.exists(&f), "old name gone");
    assert!(c.exists(&g), "new name present");
    assert_eq!(c.stat(&g).unwrap().size, 9);
}

// ---------------------------------------------------------------------------
// remove / unlink
// ---------------------------------------------------------------------------

#[test]
fn unlink_and_exists() {
    let dir = setup_dir("unlink");
    let f = format!("{}/u.txt", dir);
    let mut c = make_file(&f, b"bye");
    assert!(c.exists(&f));
    c.unlink(&f).expect("unlink");
    assert!(!c.exists(&f));
}

#[test]
fn unlinkv() {
    let dir = setup_dir("unlinkv");
    let mut c = client();
    let files = [
        format!("{}/u1", dir),
        format!("{}/u2", dir),
        format!("{}/u3", dir),
    ];
    let refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    for f in &files {
        let mut w = VfIoVec::from_path(f, 0, 1, b"z".to_vec());
        w.is_creation = true;
        c.writev(std::slice::from_mut(&mut w)).unwrap();
    }
    c.unlinkv(&refs).expect("unlinkv");
    for f in &files {
        assert!(!c.exists(f));
    }
}

#[test]
fn removev() {
    let dir = setup_dir("removev");
    let mut c = client();
    let f = format!("{}/rv.txt", dir);
    let mut w = VfIoVec::from_path(&f, 0, 1, b"r".to_vec());
    w.is_creation = true;
    c.writev(std::slice::from_mut(&mut w)).unwrap();
    c.removev(&[VfFile::from_path(&f)]).expect("removev");
    assert!(!c.exists(&f));
}

// ---------------------------------------------------------------------------
// mkdir
// ---------------------------------------------------------------------------

#[test]
fn mkdirv() {
    let dir = setup_dir("mkdirv");
    let d = format!("{}/nd", dir);
    let mut c = client();
    c.mkdirv(&[VfAttrs {
        file: VfFile::from_path(&d),
        masks: AttrMask {
            has_mode: true,
            ..AttrMask::default()
        },
        mode: 0o750,
        ..VfAttrs::default()
    }])
    .expect("mkdirv");
    let st = c.stat(&d).expect("stat dir");
    assert_eq!(st.ftype, VfType::Directory, "NF4DIR");
    assert_eq!(st.mode & 0o777, 0o750);
}

#[test]
fn mkdir() {
    let dir = setup_dir("mkdir");
    let d = format!("{}/simple", dir);
    let mut c = client();
    c.mkdir(&d, 0o755).expect("mkdir");
    assert!(c.exists(&d));
}

// ---------------------------------------------------------------------------
// symlink / readlink
// ---------------------------------------------------------------------------

#[test]
fn symlink_readlink() {
    let dir = setup_dir("symlink");
    let target = format!("{}/real.txt", dir);
    let link = format!("{}/link.txt", dir);
    let mut c = make_file(&target, b"real content");
    c.symlink(&target, &link).expect("symlink");
    let got = c.readlink(&link).expect("readlink");
    assert_eq!(String::from_utf8_lossy(&got), target);
}

#[test]
fn symlinkv_readlinkv() {
    let dir = setup_dir("symlinkv");
    let mut c = client();
    let olds: Vec<String> = (0..2).map(|i| format!("{}/src{}", dir, i)).collect();
    let news: Vec<String> = (0..2).map(|i| format!("{}/ln{}", dir, i)).collect();
    let old_refs: Vec<&str> = olds.iter().map(|s| s.as_str()).collect();
    let new_refs: Vec<&str> = news.iter().map(|s| s.as_str()).collect();
    c.symlinkv(&old_refs, &new_refs).expect("symlinkv");
    let targets = c.readlinkv(&new_refs).expect("readlinkv");
    for (t, o) in targets.iter().zip(&olds) {
        assert_eq!(String::from_utf8_lossy(t), *o);
    }
}

// ---------------------------------------------------------------------------
// hardlink
// ---------------------------------------------------------------------------

#[test]
fn hardlinkv() {
    let dir = setup_dir("hardlink");
    let src = format!("{}/orig.txt", dir);
    let dst = format!("{}/hard.txt", dir);
    let mut c = make_file(&src, b"linked data");
    let olds = [src.as_str()];
    let news = [dst.as_str()];
    c.hardlinkv(&olds, &news).expect("hardlinkv");
    assert!(c.exists(&dst));
    let s1 = c.stat(&src).unwrap();
    let s2 = c.stat(&dst).unwrap();
    assert_eq!(s1.fileid, s2.fileid, "same inode");
    assert!(s1.nlink >= 2, "nlink bumped to >= 2");
}

// ---------------------------------------------------------------------------
// ensure_dir / rm
// ---------------------------------------------------------------------------

#[test]
fn ensure_dir() {
    let dir = setup_dir("ensure_dir");
    let nested = format!("{}/a/b/c/d", dir);
    let mut c = client();
    c.ensure_dir(&nested, 0o755).expect("ensure_dir nested");
    assert!(c.exists(&format!("{}/a", dir)));
    assert!(c.exists(&nested));
}

#[test]
fn rm_recursive_api() {
    let dir = setup_dir("rm_rec");
    let mut c = client();
    c.ensure_dir(&format!("{}/x/y", dir), 0o755).unwrap();
    for f in [
        format!("{}/top", dir),
        format!("{}/x/deep", dir),
        format!("{}/x/y/deep2", dir),
    ] {
        let mut w = VfIoVec::from_path(&f, 0, 1, b"d".to_vec());
        w.is_creation = true;
        c.writev(std::slice::from_mut(&mut w)).unwrap();
    }
    assert!(c.exists(&format!("{}/x/deep", dir)));
    rm_recursive(&mut c, &dir).expect("rm_recursive");
    assert!(!c.exists(&dir), "whole tree removed");
}

#[test]
fn rm_nonrecursive_keeps_subdirs() {
    let dir = setup_dir("rm_norec");
    let mut c = client();
    let file = format!("{}/keepdir/target", dir);
    c.ensure_dir(&format!("{}/keepdir", dir), 0o755).unwrap();
    let mut w = VfIoVec::from_path(&file, 0, 1, b"k".to_vec());
    w.is_creation = true;
    c.writev(std::slice::from_mut(&mut w)).unwrap();
    // Non-recursive removal of the directory fails because it is not empty.
    let r = c.rm(&[dir.as_str()], false);
    assert!(
        r.is_err(),
        "non-empty dir cannot be removed non-recursively"
    );
    assert!(c.exists(&file));
}

// ---------------------------------------------------------------------------
// Helpers used by the new-call tests
// ---------------------------------------------------------------------------

fn write_file(c: &mut NfsVecFs, path: &str, data: &[u8]) {
    let mut w = VfIoVec::from_path(path, 0, data.len(), data.to_vec());
    w.is_creation = true;
    c.writev(std::slice::from_mut(&mut w)).unwrap();
}

fn read_all(c: &mut NfsVecFs, path: &str) -> Vec<u8> {
    let size = c.stat(path).expect("stat").size as usize;
    let mut r = VfIoVec::from_path(path, 0, size, Vec::new());
    c.readv(std::slice::from_mut(&mut r)).expect("readv");
    assert_eq!(r.length, size, "read full file");
    r.data
}

// ---------------------------------------------------------------------------
// tc_openv (per-file flags and modes)
// ---------------------------------------------------------------------------

#[test]
fn openv_per_file_flags_and_modes() {
    let dir = setup_dir("openv_flags");
    let mut c = client();
    let paths = [format!("{}/a.txt", dir), format!("{}/b.txt", dir)];
    let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    let flags = [libc::O_CREAT | libc::O_RDWR, libc::O_CREAT | libc::O_RDONLY];
    let modes = [0o600, 0o640];
    let files = c.openv(&refs, &flags, &modes).expect("openv");
    assert_eq!(files.len(), 2);
    assert_eq!(c.stat(&paths[0]).unwrap().mode & 0o777, 0o600);
    assert_eq!(c.stat(&paths[1]).unwrap().mode & 0o777, 0o640);
    c.closev(&files).unwrap();
}

// ---------------------------------------------------------------------------
// tc_fseek
// ---------------------------------------------------------------------------

#[test]
fn fseek_set_cur_end() {
    use libc::{SEEK_CUR, SEEK_END, SEEK_SET};
    let dir = setup_dir("fseek");
    let f = format!("{}/seek.bin", dir);
    let mut c = client();
    let tf = c
        .open(&f, libc::O_CREAT | libc::O_RDWR, 0o644)
        .expect("open");

    let payload = b"0123456789".to_vec();
    let mut w = VfIoVec::from_fd(tf.fd().unwrap(), 0, payload.len(), payload.clone());
    c.writev(std::slice::from_mut(&mut w)).unwrap();
    assert_eq!(c.fseek(&mut tf.clone(), 0, SEEK_END).unwrap(), 10);

    // SEEK_SET then read via VF_OFFSET_CUR.
    assert_eq!(c.fseek(&mut tf.clone(), 4, SEEK_SET).unwrap(), 4);
    let mut r = VfIoVec::from_fd(tf.fd().unwrap(), VF_OFFSET_CUR, 6, Vec::new());
    c.readv(std::slice::from_mut(&mut r)).unwrap();
    assert_eq!(r.data, b"456789", "read at current offset after fseek");

    // SEEK_CUR advances from the tracked offset (4 + 6 = 10).
    assert_eq!(c.fseek(&mut tf.clone(), -4, SEEK_CUR).unwrap(), 6);
    let mut r = VfIoVec::from_fd(tf.fd().unwrap(), VF_OFFSET_CUR, 4, Vec::new());
    c.readv(std::slice::from_mut(&mut r)).unwrap();
    assert_eq!(r.data, b"6789");

    c.close(&tf).unwrap();
}

// ---------------------------------------------------------------------------
// tc_dupv / tc_copyv (extent copy)
// ---------------------------------------------------------------------------

#[test]
fn dupv_copies_extent() {
    let dir = setup_dir("dupv");
    let src = format!("{}/src.bin", dir);
    let dst = format!("{}/dst.bin", dir);
    let mut c = client();
    write_file(&mut c, &src, b"abcdefghij");

    let pairs = [ExtentPair::new(&src, 4, &dst, 0, 4)];
    c.dupv(&pairs).expect("dupv");
    assert_eq!(read_all(&mut c, &dst), b"efgh");

    // VF_EXTENT length u64::MAX copies to end-of-file.
    let whole = format!("{}/whole.bin", dir);
    c.copyv(&[ExtentPair::new(&src, 2, &whole, 0, u64::MAX)])
        .expect("copyv whole file");
    assert_eq!(read_all(&mut c, &whole), b"cdefghij");
}

#[test]
fn ldupv_and_lcopyv() {
    let dir = setup_dir("ldupv");
    let src = format!("{}/s.txt", dir);
    let d1 = format!("{}/d1.txt", dir);
    let d2 = format!("{}/d2.txt", dir);
    let mut c = client();
    write_file(&mut c, &src, b"0123456789");
    c.ldupv(&[ExtentPair::new(&src, 0, &d1, 0, 5)]).unwrap();
    c.lcopyv(&[ExtentPair::new(&src, 5, &d2, 0, 5)]).unwrap();
    assert_eq!(read_all(&mut c, &d1), b"01234");
    assert_eq!(read_all(&mut c, &d2), b"56789");
}

// ---------------------------------------------------------------------------
// tc_write_adb
// ---------------------------------------------------------------------------

#[test]
fn write_adb_blocknums_and_pattern() {
    let dir = setup_dir("adb");
    let f = format!("{}/adb.bin", dir);
    let mut c = client();

    // Three ADB blocks of 1024 bytes; write the ADBN (8 bytes, BE) at the
    // start of each block, and the pattern "PAT" 8 bytes into each block.
    let mut a = Adb {
        path: f.clone(),
        adb_offset: 0,
        adb_block_size: 1024,
        adb_block_count: 3,
        adb_reloff_blocknum: 0,
        adb_block_num: 100,
        adb_reloff_pattern: 8,
        adb_pattern_size: 3,
        adb_pattern_data: b"PAT".to_vec(),
    };
    c.write_adb(std::slice::from_mut(&mut a))
        .expect("write_adb");
    assert_eq!(a.adb_block_count, 3, "all blocks written");

    for (i, expected_adbn) in [100u64, 101, 102].iter().enumerate() {
        let base = i as u64 * 1024;
        let mut bn = VfIoVec::from_path(&f, base, 8, Vec::new());
        c.readv(std::slice::from_mut(&mut bn)).unwrap();
        assert_eq!(
            u64::from_be_bytes(bn.data[0..8].try_into().unwrap()),
            *expected_adbn,
            "ADBN of block {}",
            i
        );
        let mut pt = VfIoVec::from_path(&f, base + 8, 3, Vec::new());
        c.readv(std::slice::from_mut(&mut pt)).unwrap();
        assert_eq!(pt.data, b"PAT", "pattern of block {}", i);
    }
}

// ---------------------------------------------------------------------------
// tc_listdirv (callback)
// ---------------------------------------------------------------------------

#[test]
fn listdirv_callback() {
    let dir = setup_dir("listdirv");
    let mut c = client();
    c.ensure_dir(&format!("{}/sub", dir), 0o755).unwrap();
    for name in ["a.txt", "b.txt"] {
        write_file(&mut c, &format!("{}/{}", dir, name), b"x");
    }

    let mut seen: Vec<String> = Vec::new();
    let mut cb = |e: &VfAttrs, d: &str| {
        assert_eq!(d, dir);
        seen.push(e.file.path().unwrap().to_string_lossy().into_owned());
        true
    };
    c.listdirv(&[dir.as_str()], AttrMask::default(), 0, false, &mut cb)
        .expect("listdirv");
    assert!(seen.iter().any(|p| p.ends_with("a.txt")));
    assert!(seen.iter().any(|p| p.ends_with("sub")));

    // A callback returning false stops early.
    let mut count = 0usize;
    let mut stop = |_: &VfAttrs, _: &str| {
        count += 1;
        false
    };
    c.listdirv(&[dir.as_str()], AttrMask::default(), 0, false, &mut stop)
        .unwrap();
    assert_eq!(count, 1, "early-stop after first entry");
}

// ---------------------------------------------------------------------------
// tc_cp_recursive
// ---------------------------------------------------------------------------

#[test]
fn cp_recursive_copies_tree() {
    let dir = setup_dir("cp_rec");
    let src = format!("{}/src", dir);
    let dst = format!("{}/dst", dir);
    let mut c = client();
    c.ensure_dir(&format!("{}/sub", src), 0o755).unwrap();
    write_file(&mut c, &format!("{}/a.txt", src), b"aaa");
    write_file(&mut c, &format!("{}/sub/b.txt", src), b"bbbb");

    c.cp_recursive(&src, &dst, true, false)
        .expect("cp_recursive");
    assert_eq!(read_all(&mut c, &format!("{}/a.txt", dst)), b"aaa");
    assert_eq!(read_all(&mut c, &format!("{}/sub/b.txt", dst)), b"bbbb");
}

// ---------------------------------------------------------------------------
// Compound batching
// ---------------------------------------------------------------------------

#[test]
fn resolve_deep_path_single_compound() {
    // A 6-component path must resolve (and read/write) correctly; resolve now
    // walks the whole path in one [PUTFH root, LOOKUP x6, GETFH] compound.
    let dir = setup_dir("resolve_deep");
    let deep = format!("{}/a/b/c/d/e", dir);
    let mut c = client();
    c.ensure_dir(&deep, 0o755).unwrap();
    write_file(&mut c, &format!("{}/file.txt", deep), b"deep");
    assert_eq!(read_all(&mut c, &format!("{}/file.txt", deep)), b"deep");
}

#[test]
fn batched_readv_writev_many_files() {
    let dir = setup_dir("batch_rw");
    let mut c = client();

    // Open five files; batched WRITEs and REAADs go through the descriptor
    // fast path (one compound per chunk of `[PUTFH, WRITE/READ]` pairs).
    let mut tfs = Vec::new();
    for i in 0..5u8 {
        let f = format!("{}/f{}.txt", dir, i);
        tfs.push(c.open(&f, libc::O_CREAT | libc::O_RDWR, 0o644).unwrap());
    }

    let mut writes: Vec<VfIoVec> = tfs
        .iter()
        .map(|tf| {
            VfIoVec::from_fd(
                tf.fd().unwrap(),
                0,
                3,
                vec![b'a' + (tf.fd().unwrap() - 1) as u8; 3],
            )
        })
        .collect();
    c.writev(&mut writes).expect("batched writev");
    for (i, w) in writes.iter().enumerate() {
        assert_eq!(w.length, 3, "write {} wrote 3 bytes", i);
    }

    let mut reads: Vec<VfIoVec> = tfs
        .iter()
        .map(|tf| VfIoVec::from_fd(tf.fd().unwrap(), 0, 3, Vec::new()))
        .collect();
    c.readv(&mut reads).expect("batched readv");
    for (i, r) in reads.iter().enumerate() {
        let expect = vec![b'a' + i as u8; 3];
        assert_eq!(r.data, expect, "read {} content", i);
        assert!(!r.is_failure);
    }

    c.closev(&tfs).unwrap();
}

#[test]
fn batched_unlinkv() {
    // unlinkv groups same-parent paths into one compound of REMOVEs.
    let dir = setup_dir("batch_unlink");
    let mut c = client();
    let files = [
        format!("{}/u1", dir),
        format!("{}/u2", dir),
        format!("{}/u3", dir),
    ];
    for f in &files {
        write_file(&mut c, f, b"x");
    }
    let refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    c.unlinkv(&refs).expect("batched unlinkv");
    for f in &files {
        assert!(!c.exists(f));
    }
}

#[test]
fn batch_exceeds_compound_op_limit() {
    // 10 files: getattrsv / setattrsv / openv / readv / writev / closev all
    // batch, and 10 ops must be split across multiple compounds (the server
    // grants ca_maxoperations=16, and each file costs 2-3 ops + SEQUENCE).
    let dir = setup_dir("batch_many");
    let mut c = client();
    let n = 10u8;
    let mut paths = Vec::new();
    let mut files = Vec::new();
    for i in 0..n {
        let p = format!("{}/f{}.txt", dir, i);
        let tf = c
            .open(&p, libc::O_CREAT | libc::O_RDWR, 0o600)
            .expect("open");
        paths.push(p);
        files.push(tf);
    }

    // Batched writev across all 10 open files.
    let mut writes: Vec<VfIoVec> = files
        .iter()
        .map(|tf| {
            VfIoVec::from_fd(
                tf.fd().unwrap(),
                0,
                2,
                vec![b'a' + (tf.fd().unwrap() - 1) as u8; 2],
            )
        })
        .collect();
    c.writev(&mut writes).expect("batched writev (10 files)");

    // Batched getattrsv / setattrsv on all 10 paths.
    let mut attrs: Vec<VfAttrs> = paths
        .iter()
        .map(|p| VfAttrs {
            file: VfFile::from_path(p),
            masks: AttrMask {
                has_mode: true,
                has_size: true,
                ..AttrMask::default()
            },
            ..VfAttrs::default()
        })
        .collect();
    c.getattrsv(&mut attrs).expect("getattrsv (10 files)");
    for a in &attrs {
        assert_eq!(a.size, 2);
        assert_eq!(a.mode & 0o777, 0o600);
    }
    c.setattrsv(&attrs).expect("setattrsv (10 files)");

    // Batched readv back.
    let mut reads: Vec<VfIoVec> = files
        .iter()
        .map(|tf| VfIoVec::from_fd(tf.fd().unwrap(), 0, 2, Vec::new()))
        .collect();
    c.readv(&mut reads).expect("batched readv (10 files)");
    for (i, r) in reads.iter().enumerate() {
        assert_eq!(r.data, vec![b'a' + i as u8; 2], "read {} content", i);
    }

    // Batched closev.
    c.closev(&files).expect("closev (10 files)");
    c.unlinkv(&paths.iter().map(|p| p.as_str()).collect::<Vec<_>>())
        .expect("unlinkv (10 files)");
}
