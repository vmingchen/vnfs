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
        Err(e) => assert_eq!(e.err_no(), nfsstat4_NFS4ERR_EXIST, "got {:?}", e),
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

#[test]
fn openv_append_writes_at_end() {
    let dir = setup_dir("openv_append");
    let paths = [
        format!("{}/a.txt", dir),
        format!("{}/b.txt", dir),
        format!("{}/c.txt", dir),
    ];
    let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    let mut c = client();
    let files = c
        .openv_simple(&refs, libc::O_CREAT | libc::O_RDWR | libc::O_APPEND, 0o644)
        .expect("openv_simple append");
    for f in &files {
        c.writev(&[WriteOp::new(f.clone(), VfOffset::At(0), b"ab".to_vec())])
            .unwrap();
        c.writev(&[WriteOp::new(f.clone(), VfOffset::At(0), b"cd".to_vec())])
            .unwrap();
    }
    c.closev(&files).expect("closev");
    for p in &paths {
        assert_eq!(c.stat(p).unwrap().size, 4, "append via openv");
        assert_eq!(c.read(&VfFile::from_path(p), 0, 4).unwrap(), b"abcd");
    }
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
    let wr = &c
        .writev(&[WriteOp::from_path(&f, VfOffset::At(0), payload.clone()).with_creation()])
        .expect("writev")[0];
    assert_eq!(wr.written, payload.len(), "all bytes written");

    let r = &c
        .readv(&[ReadOp::from_path(&f, VfOffset::At(0), payload.len())])
        .expect("readv")[0];
    assert_eq!(r.data, payload, "read back what was written");
}

#[test]
fn readv_eof() {
    let dir = setup_dir("readv_eof");
    let f = format!("{}/short.txt", dir);
    let mut c = client();
    let payload = b"abcdefghij".to_vec();
    c.writev(&[WriteOp::from_path(&f, VfOffset::At(0), payload.clone()).with_creation()])
        .unwrap();

    // Ask for more than exists: must not fail and must hit EOF.
    let r = &c
        .readv(&[ReadOp::from_path(&f, VfOffset::At(0), 100)])
        .expect("readv")[0];
    assert_eq!(r.data.len(), payload.len());
    assert!(r.eof, "short read must set eof");
}

#[test]
fn readv_multiple_files() {
    let dir = setup_dir("readv_multi");
    let mut c = client();
    let mut iovs = Vec::new();
    for (i, name) in ["x", "y", "z"].iter().enumerate() {
        let f = format!("{}/{}.txt", dir, name);
        let payload = vec![b'a' + i as u8; 8];
        c.writev(&[WriteOp::from_path(&f, VfOffset::At(0), payload).with_creation()])
            .unwrap();
        let r = &c
            .readv(&[ReadOp::from_path(&f, VfOffset::At(0), 8)])
            .unwrap()[0];
        iovs.push(r.data.clone());
    }
    assert_eq!(iovs[0], vec![b'a'; 8]);
    assert_eq!(iovs[1], vec![b'b'; 8]);
    assert_eq!(iovs[2], vec![b'c'; 8]);
}

// ---------------------------------------------------------------------------
// getattrs / stat / lstat / fstat / exists
// ---------------------------------------------------------------------------

fn make_file(path: &str, content: &[u8]) -> NfsVecFs {
    let mut c = client();
    c.writev(&[WriteOp::from_path(path, VfOffset::At(0), content.to_vec()).with_creation()])
        .unwrap();
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
    assert!(c.exists(&f).unwrap());
    assert!(!c.exists(&format!("{}/missing.txt", dir)).unwrap());
}

#[test]
fn getattrsv() {
    let dir = setup_dir("getattrsv");
    let mut c = client();
    let mut attrs = Vec::new();
    for name in ["g0", "g1", "g2"] {
        let f = format!("{}/{}.txt", dir, name);
        c.writev(&[WriteOp::from_path(&f, VfOffset::At(0), vec![b'x'; 3]).with_creation()])
            .unwrap();
        attrs.push(VfAttrs {
            file: VfFile::from_path(&f),
            masks: AttrMask::MODE | AttrMask::SIZE | AttrMask::NLINK | AttrMask::FILEID,
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
        masks: AttrMask::MODE,
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
        masks: AttrMask::SIZE,
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
        masks: AttrMask::MODE,
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
        c.writev(&[
            WriteOp::from_path(&f, VfOffset::At(0), vec![b'a' + i as u8; 4]).with_creation(),
        ])
        .unwrap();
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
        c.writev(&[WriteOp::from_path(&f, VfOffset::At(0), b"ok".to_vec()).with_creation()])
            .unwrap();
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
    assert!(!c.exists(&f).unwrap(), "old name gone");
    assert!(c.exists(&g).unwrap(), "new name present");
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
    assert!(c.exists(&f).unwrap());
    c.unlink(&f).expect("unlink");
    assert!(!c.exists(&f).unwrap());
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
        c.writev(&[WriteOp::from_path(f, VfOffset::At(0), b"z".to_vec()).with_creation()])
            .unwrap();
    }
    c.unlinkv(&refs).expect("unlinkv");
    for f in &files {
        assert!(!c.exists(f).unwrap());
    }
}

#[test]
fn removev() {
    let dir = setup_dir("removev");
    let mut c = client();
    let f = format!("{}/rv.txt", dir);
    c.writev(&[WriteOp::from_path(&f, VfOffset::At(0), b"r".to_vec()).with_creation()])
        .unwrap();
    c.removev(&[VfFile::from_path(&f)]).expect("removev");
    assert!(!c.exists(&f).unwrap());
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
        masks: AttrMask::MODE,
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
    assert!(c.exists(&d).unwrap());
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

#[test]
fn intermediate_symlink_components_followed() {
    let dir = setup_dir("intermediate_link");
    let mut c = client();
    c.mkdir(&format!("{}/realdir", dir), 0o755).unwrap();
    write_file(&mut c, &format!("{}/realdir/file", dir), b"data");
    c.symlink("realdir", &format!("{}/dirlink", dir)).unwrap();

    let st = c.stat(&format!("{}/dirlink/file", dir)).unwrap();
    assert_eq!(st.ftype, VfType::Regular);
    assert_eq!(st.size, 4);
    assert_eq!(read_all(&mut c, &format!("{}/dirlink/file", dir)), b"data");

    // Path-based operations (unlink) resolve through the intermediate link.
    let f = format!("{}/dirlink/other", dir);
    write_file(&mut c, &f, b"x");
    c.unlink(&f).unwrap();
    assert!(!c.exists(&f).unwrap());

    // lstat of a path under the link still reports the final object.
    assert_eq!(
        c.lstat(&format!("{}/dirlink/file", dir)).unwrap().ftype,
        VfType::Regular
    );
}

#[test]
fn path_readv_is_batched() {
    let dir = setup_dir("path_batch");
    let mut c = client();
    let paths: Vec<String> = (0..5)
        .map(|i| {
            let p = format!("{}/f{}", dir, i);
            write_file(&mut c, &p, b"x");
            p
        })
        .collect();
    let _ = vnfs::compound::compound_stats(); // reset counters
    let ops: Vec<ReadOp> = paths
        .iter()
        .map(|p| ReadOp::at(VfFile::from_path(p), 0, 1))
        .collect();
    let res = c.readv(&ops).unwrap();
    assert_eq!(res.len(), 5);
    let compounds = vnfs::compound::compound_stats().0;
    // Batching must beat the per-op open+read+close round trips.
    assert!(
        compounds < 3 * paths.len() as u64,
        "path readv used {} compounds for {} reads",
        compounds,
        paths.len()
    );
}

#[test]
fn openv_ocreat_preserves_existing_mode() {
    let dir = setup_dir("openv_mode");
    let mut c = client();
    let f = format!("{}/existing.txt", dir);
    let fd = c.open(&f, libc::O_CREAT | libc::O_RDWR, 0o600).unwrap();
    c.close(&fd).unwrap();

    let g = format!("{}/new.txt", dir);
    c.openv(
        &[f.as_str(), g.as_str()],
        &[libc::O_CREAT | libc::O_RDWR, libc::O_CREAT | libc::O_RDWR],
        &[0o777, 0o640],
    )
    .unwrap();
    assert_eq!(
        c.stat(&f).unwrap().mode & 0o7777,
        0o600,
        "openv O_CREAT must not chmod an existing file"
    );
    assert_eq!(c.stat(&g).unwrap().mode & 0o7777, 0o640, "new file mode");
}

#[test]
fn transport_error_on_unreachable_server() {
    let e = match NfsVecFs::connect("127.0.0.1:1") {
        Ok(_) => panic!("must fail to connect"),
        Err(e) => e,
    };
    assert!(e.is_transport(), "unreachable server is a transport error");
    assert_eq!(e.err_no(), VF_ERR_RPC);
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
    assert!(c.exists(&dst).unwrap());
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
    assert!(c.exists(&format!("{}/a", dir)).unwrap());
    assert!(c.exists(&nested).unwrap());
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
        c.writev(&[WriteOp::from_path(&f, VfOffset::At(0), b"d".to_vec()).with_creation()])
            .unwrap();
    }
    assert!(c.exists(&format!("{}/x/deep", dir)).unwrap());
    rm_recursive(&mut c, &dir).expect("rm_recursive");
    assert!(!c.exists(&dir).unwrap(), "whole tree removed");
}

#[test]
fn rm_nonrecursive_keeps_subdirs() {
    let dir = setup_dir("rm_norec");
    let mut c = client();
    let file = format!("{}/keepdir/target", dir);
    c.ensure_dir(&format!("{}/keepdir", dir), 0o755).unwrap();
    c.writev(&[WriteOp::from_path(&file, VfOffset::At(0), b"k".to_vec()).with_creation()])
        .unwrap();
    // Non-recursive removal of the directory fails because it is not empty.
    let r = c.rm(&[dir.as_str()], false);
    assert!(
        r.is_err(),
        "non-empty dir cannot be removed non-recursively"
    );
    assert!(c.exists(&file).unwrap());
}

// ---------------------------------------------------------------------------
// Helpers used by the new-call tests
// ---------------------------------------------------------------------------

fn write_file(c: &mut NfsVecFs, path: &str, data: &[u8]) {
    c.writev(&[WriteOp::from_path(path, VfOffset::At(0), data.to_vec()).with_creation()])
        .unwrap();
}

fn read_all(c: &mut NfsVecFs, path: &str) -> Vec<u8> {
    let size = c.stat(path).expect("stat").size as usize;
    let r = &c
        .readv(&[ReadOp::from_path(path, VfOffset::At(0), size)])
        .expect("readv")[0];
    assert_eq!(r.data.len(), size, "read full file");
    r.data.clone()
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
    let dir = setup_dir("fseek");
    let f = format!("{}/seek.bin", dir);
    let mut c = client();
    let tf = c
        .open(&f, libc::O_CREAT | libc::O_RDWR, 0o644)
        .expect("open");

    let payload = b"0123456789".to_vec();
    c.writev(&[WriteOp::from_fd(
        tf.fd().unwrap(),
        VfOffset::At(0),
        payload.clone(),
    )])
    .unwrap();
    assert_eq!(c.fseek(&tf, 0, SeekFrom::End).unwrap(), 10);

    // SeekFrom::Set then read via VfOffset::Cur.
    assert_eq!(c.fseek(&tf, 4, SeekFrom::Set).unwrap(), 4);
    let r = &c
        .readv(&[ReadOp::from_fd(tf.fd().unwrap(), VfOffset::Cur, 6)])
        .unwrap()[0];
    assert_eq!(r.data, b"456789", "read at current offset after fseek");

    // SeekFrom::Cur advances from the tracked offset (4 + 6 = 10).
    assert_eq!(c.fseek(&tf, -4, SeekFrom::Cur).unwrap(), 6);
    let r = &c
        .readv(&[ReadOp::from_fd(tf.fd().unwrap(), VfOffset::Cur, 4)])
        .unwrap()[0];
    assert_eq!(r.data, b"6789");

    // Seeking before the start is refused.
    assert!(c.fseek(&tf, -100, SeekFrom::Set).is_err());
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
    let a = Adb {
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
    let counts = c.write_adb(&[a]).expect("write_adb");
    assert_eq!(counts[0], 3, "all blocks written");

    for (i, expected_adbn) in [100u64, 101, 102].iter().enumerate() {
        let base = i as u64 * 1024;
        let bn = &c
            .readv(&[ReadOp::from_path(&f, VfOffset::At(base), 8)])
            .unwrap()[0];
        assert_eq!(
            u64::from_be_bytes(bn.data[0..8].try_into().unwrap()),
            *expected_adbn,
            "ADBN of block {}",
            i
        );
        let pt = &c
            .readv(&[ReadOp::from_path(&f, VfOffset::At(base + 8), 3)])
            .unwrap()[0];
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

    let writes: Vec<WriteOp> = tfs
        .iter()
        .map(|tf| {
            WriteOp::from_fd(
                tf.fd().unwrap(),
                VfOffset::At(0),
                vec![b'a' + (tf.fd().unwrap() - 1) as u8; 3],
            )
        })
        .collect();
    let wres = c.writev(&writes).expect("batched writev");
    for (i, w) in wres.iter().enumerate() {
        assert_eq!(w.written, 3, "write {} wrote 3 bytes", i);
    }

    let reads: Vec<ReadOp> = tfs
        .iter()
        .map(|tf| ReadOp::from_fd(tf.fd().unwrap(), VfOffset::At(0), 3))
        .collect();
    let rres = c.readv(&reads).expect("batched readv");
    for (i, r) in rres.iter().enumerate() {
        let expect = vec![b'a' + i as u8; 3];
        assert_eq!(r.data, expect, "read {} content", i);
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
        assert!(!c.exists(f).unwrap());
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
    let writes: Vec<WriteOp> = files
        .iter()
        .map(|tf| {
            WriteOp::from_fd(
                tf.fd().unwrap(),
                VfOffset::At(0),
                vec![b'a' + (tf.fd().unwrap() - 1) as u8; 2],
            )
        })
        .collect();
    c.writev(&writes).expect("batched writev (10 files)");

    // Batched getattrsv / setattrsv on all 10 paths.
    let mut attrs: Vec<VfAttrs> = paths
        .iter()
        .map(|p| VfAttrs {
            file: VfFile::from_path(p),
            masks: AttrMask::MODE | AttrMask::SIZE,
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
    let reads: Vec<ReadOp> = files
        .iter()
        .map(|tf| ReadOp::from_fd(tf.fd().unwrap(), VfOffset::At(0), 2))
        .collect();
    let rres = c.readv(&reads).expect("batched readv (10 files)");
    for (i, r) in rres.iter().enumerate() {
        assert_eq!(r.data, vec![b'a' + i as u8; 2], "read {} content", i);
    }

    // Batched closev.
    c.closev(&files).expect("closev (10 files)");
    c.unlinkv(&paths.iter().map(|p| p.as_str()).collect::<Vec<_>>())
        .expect("unlinkv (10 files)");
}
