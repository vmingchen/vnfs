//! A capability-aware shared test suite run against every [`VecFs`]
//! implementation. Portable behavior must match on all backends; optional
//! Unix semantics are asserted only when advertised.

use std::path::{Path, PathBuf};
use vnfs::VecFs;
use vnfs::vecfs::*;

/// Run a broad set of vectorized-filesystem assertions against `fs`, using
/// paths under `base` (which must be unique per caller).
pub fn run_suite(fs: &mut impl VecFs, base: &str) {
    let capabilities = fs.capabilities();
    let posix_metadata = capabilities & VF_CAP_POSIX_METADATA != 0;
    let symlinks = capabilities & VF_CAP_SYMLINKS != 0;
    let hardlinks = capabilities & VF_CAP_HARDLINKS != 0;
    let dir = format!("{}/suite", base);
    fs.ensure_dir(Path::new(&dir), 0o755).expect("ensure_dir");
    let f = format!("{}/f.txt", dir);

    // writev / readv via paths.
    let payload = b"the quick brown fox jumps over the lazy dog".to_vec();
    let mut w = WriteOp::from_path(&f, VfOffset::At(0), payload.clone());
    w.creation = true;
    let wr = &fs.writev(&[w]).expect("writev")[0];
    assert_eq!(wr.written, payload.len());

    let r = &fs
        .readv(&[ReadOp::from_path(&f, VfOffset::At(0), payload.len())])
        .expect("readv")[0];
    assert_eq!(r.data, payload);
    // eof is the backend's EOF signal: NFS reports it for an exact-to-EOF
    // read, the std::fs backend only for short reads. Either way no more data
    // is available, so only the data itself is asserted here.

    // stat / exists / file_type.
    let st = fs.stat(Path::new(&f)).expect("stat");
    assert_eq!(st.size, payload.len() as u64);
    if posix_metadata {
        assert!(st.fileid != 0);
        assert!(st.nlink >= 1, "nlink");
        assert_eq!(st.rdev, 0, "rdev of a regular file");
    }
    // Time attributes are populated (reported in `returned`) on both
    // backends; exact values depend on the filesystem clock semantics.
    if posix_metadata {
        let mut t = VfAttrs {
            file: VfFile::from_path(&f),
            masks: AttrMask::ATIME | AttrMask::MTIME | AttrMask::CTIME,
            ..VfAttrs::default()
        };
        fs.getattrsv(std::slice::from_mut(&mut t)).unwrap();
        assert_eq!(
            t.returned,
            AttrMask::ATIME | AttrMask::MTIME | AttrMask::CTIME,
            "time attributes returned"
        );
    }
    // A 4 KiB file occupies at least one 512-byte block on both backends
    // (tmpfs reports 0 blocks for tiny files).
    if posix_metadata {
        let big = format!("{}/big.bin", dir);
        fs.writev(&[WriteOp::at(VfFile::from_path(&big), 0, vec![b'x'; 4096]).with_creation()])
            .unwrap();
        let mut b = VfAttrs {
            file: VfFile::from_path(&big),
            masks: AttrMask::BLOCKS,
            ..VfAttrs::default()
        };
        fs.getattrsv(std::slice::from_mut(&mut b)).unwrap();
        assert!(b.returned.contains(AttrMask::BLOCKS), "blocks returned");
        assert!(b.blocks > 0, "blocks in 512-byte units");
    }
    assert!(fs.exists(Path::new(&f)).unwrap());
    assert_eq!(fs.file_type(Path::new(&f)).unwrap(), VfType::Regular);

    // setattrs: truncate to 5 bytes, then mode.
    fs.setattrsv(&[VfAttrs {
        file: VfFile::from_path(&f),
        masks: AttrMask::SIZE,
        size: 5,
        ..VfAttrs::default()
    }])
    .expect("setattrsv size");
    assert_eq!(fs.stat(Path::new(&f)).unwrap().size, 5);

    // mkdir.
    let sub = format!("{}/sub", dir);
    fs.mkdir(Path::new(&sub), 0o750).expect("mkdir");
    assert_eq!(fs.stat(Path::new(&sub)).unwrap().ftype, VfType::Directory);
    if posix_metadata {
        assert_eq!(fs.stat(Path::new(&sub)).unwrap().mode & 0o777, 0o750);
    }

    // open / descriptor write / fseek / descriptor read.
    let tf = fs.open(Path::new(&f), libc::O_RDWR, 0).expect("open");
    fs.writev(&[WriteOp::from_fd(
        tf.fd().unwrap(),
        VfOffset::At(0),
        b"hello".to_vec(),
    )])
    .expect("writev fd");
    assert_eq!(fs.fseek(&tf, 0, SeekFrom::Set).unwrap(), 0);
    let r = &fs
        .readv(&[ReadOp::from_fd(tf.fd().unwrap(), VfOffset::Cur, 5)])
        .expect("readv fd")[0];
    assert_eq!(r.data, b"hello");
    fs.close(&tf).expect("close");

    if symlinks {
        // symlink / readlink.
        let link = format!("{}/ln", dir);
        fs.symlink(Path::new(&f), Path::new(&link))
            .expect("symlink");
        assert_eq!(fs.readlink(Path::new(&link)).unwrap(), f.as_bytes());

        // stat follows symlinks; lstat does not. (Relative target so both the
        // NFS and std::fs backends can resolve it.)
        let sbase = format!("{}/lnstat", dir);
        let rel = std::path::Path::new(&f)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        fs.symlink(Path::new(&rel), Path::new(&sbase))
            .expect("symlink lnstat");
        assert_eq!(
            fs.stat(Path::new(&sbase)).unwrap().ftype,
            VfType::Regular,
            "stat follows"
        );
        assert_eq!(
            fs.lstat(Path::new(&sbase)).unwrap().ftype,
            VfType::Symlink,
            "lstat stays"
        );
        assert_eq!(
            fs.stat(Path::new(&sbase)).unwrap().size,
            fs.stat(Path::new(&f)).unwrap().size,
            "stat resolves to the target"
        );
    }

    // hardlink.
    if hardlinks {
        let hard = format!("{}/hard", dir);
        fs.hardlinkv(&[Path::new(&f)], &[Path::new(&hard)])
            .expect("hardlinkv");
        assert_eq!(
            fs.stat(Path::new(&f)).unwrap().fileid,
            fs.stat(Path::new(&hard)).unwrap().fileid
        );
    }

    // rename.
    let renamed = format!("{}/renamed.txt", dir);
    fs.renamev(&[(
        VfFile::from_os_path(Path::new(&f)),
        VfFile::from_os_path(Path::new(&renamed)),
    )])
    .expect("renamev");
    assert!(fs.exists(Path::new(&renamed)).unwrap());
    assert!(!fs.exists(Path::new(&f)).unwrap());

    // openv / closev.
    let more = format!("{}/more", dir);
    fs.ensure_dir(Path::new(&more), 0o755).unwrap();
    let paths = [
        format!("{}/a", more),
        format!("{}/b", more),
        format!("{}/c", more),
    ];
    let refs: Vec<&Path> = paths.iter().map(Path::new).collect();
    let files = fs
        .openv(&refs, &[libc::O_CREAT | libc::O_RDWR; 3], &[0o644; 3])
        .expect("openv");
    fs.closev(&files).expect("closev");

    // listdir (non-recursive) finds entries.
    let entries = fs
        .listdir(Path::new(&dir), AttrMask::default(), 0, false)
        .expect("listdir");
    assert!(
        entries
            .iter()
            .any(|e| e.file.path().unwrap().ends_with("renamed.txt"))
    );
    assert!(
        entries
            .iter()
            .any(|e| e.file.path().unwrap().ends_with("sub"))
    );

    // listdirv callback.
    let mut seen = 0usize;
    let mut cb = |_: &VfAttrs, _: &Path| {
        seen += 1;
        true
    };
    fs.listdirv(&[Path::new(&dir)], AttrMask::default(), 0, false, &mut cb)
        .expect("listdirv");
    assert!(seen >= 2);

    // dupv extent copy (whole file).
    let copy = format!("{}/copy.txt", dir);
    fs.dupv(&[ExtentPair::new(&renamed, 0, &copy, 0, None)])
        .expect("dupv");
    assert_eq!(
        fs.stat(Path::new(&copy)).unwrap().size,
        fs.stat(Path::new(&renamed)).unwrap().size
    );

    // dupv over a longer existing destination truncates the stale tail.
    let short = format!("{}/short.txt", dir);
    let long = format!("{}/long.txt", dir);
    fs.writev(&[WriteOp::at(VfFile::from_path(&short), 0, b"ab".to_vec()).with_creation()])
        .unwrap();
    fs.writev(&[WriteOp::at(VfFile::from_path(&long), 0, b"abcdef".to_vec()).with_creation()])
        .unwrap();
    fs.dupv(&[ExtentPair::new(&short, 0, &long, 0, None)])
        .unwrap();
    let st = fs.stat(Path::new(&long)).unwrap();
    assert_eq!(st.size, 2, "dupv truncates the destination");
    assert_eq!(fs.read(&VfFile::from_path(&long), 0, 8).unwrap(), b"ab");

    // O_CREAT mode is ignored when the file already exists.
    let mode_f = format!("{}/mode.txt", dir);
    let mfd = fs
        .open(Path::new(&mode_f), libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    fs.close(&mfd).unwrap();
    let mfd = fs
        .open(Path::new(&mode_f), libc::O_CREAT | libc::O_RDWR, 0o777)
        .unwrap();
    fs.close(&mfd).unwrap();
    if posix_metadata {
        assert_eq!(
            fs.stat(Path::new(&mode_f)).unwrap().mode & 0o7777,
            0o600,
            "O_CREAT must not chmod an existing file"
        );
    }

    // O_CREAT | O_EXCL fails on an existing file; O_TRUNC empties it.
    assert_eq!(
        fs.open(
            Path::new(&mode_f),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o644
        )
        .unwrap_err()
        .err_no(),
        ERR_EXIST
    );
    let tfd = fs
        .open(Path::new(&mode_f), libc::O_RDWR | libc::O_TRUNC, 0)
        .unwrap();
    fs.close(&tfd).unwrap();
    assert_eq!(
        fs.stat(Path::new(&mode_f)).unwrap().size,
        0,
        "O_TRUNC empties the file"
    );

    // "Current position" offsets are invalid for path-based operations.
    assert_eq!(
        fs.readv(&[ReadOp::new(VfFile::from_path(&renamed), VfOffset::Cur, 1)])
            .unwrap_err()
            .err_no(),
        ERR_INVAL
    );

    // Mixed descriptor/path batches work and echo the original files.
    let rfd = fs.open(Path::new(&renamed), libc::O_RDONLY, 0).unwrap();
    let mixed = fs
        .readv(&[
            ReadOp::new(rfd.clone(), VfOffset::At(0), 2),
            ReadOp::at(VfFile::from_path(&copy), 0, 2),
        ])
        .unwrap();
    assert_eq!(mixed[0].data.len(), 2);
    assert_eq!(mixed[1].data.len(), 2);
    assert!(mixed[0].file.is_descriptor(), "result echoes the fd op");
    assert!(mixed[1].file.path().is_some(), "result echoes the path op");
    fs.close(&rfd).unwrap();

    // A closed descriptor inside a batch fails at its own index with EBADF.
    let bad = fs.open(Path::new(&renamed), libc::O_RDONLY, 0).unwrap();
    fs.close(&bad).unwrap();
    let e = fs
        .readv(&[
            ReadOp::at(VfFile::from_path(&copy), 0, 1),
            ReadOp::new(bad, VfOffset::At(0), 1),
        ])
        .unwrap_err();
    assert_eq!((e.index(), e.err_no()), (1, ERR_EBADF));

    // readlink/hardlink error paths.
    assert!(
        fs.readlink(Path::new(&renamed)).is_err(),
        "readlink of a regular file"
    );
    assert!(
        fs.hardlinkv(
            &[Path::new("/no/such/source")],
            &[Path::new(&format!("{}/h", dir))]
        )
        .is_err(),
        "hardlink of a missing source"
    );

    // ensure_dir on an existing directory is a no-op.
    fs.ensure_dir(Path::new(&sub), 0o755)
        .expect("ensure_dir existing");

    // Non-recursive rm of a non-empty directory fails.
    let nonempty = format!("{}/nonempty", dir);
    fs.ensure_dir(Path::new(&nonempty), 0o755).unwrap();
    fs.writev(&[WriteOp::at(
        VfFile::from_os_path(Path::new(&format!("{}/x", nonempty))),
        0,
        b"x".to_vec(),
    )
    .with_creation()])
        .unwrap();
    assert!(fs.rm(&[Path::new(&nonempty)], false).is_err());

    // write_adb: two blocks with ADBN at block offset 0.
    let adbf = format!("{}/adb.bin", dir);
    let mut a = Adb::blocknum_only(&adbf, 0, 1024, 2, 0, 100);
    a.adb_reloff_pattern = Some(8);
    a.adb_pattern_data = b"PAT".to_vec();
    fs.write_adb(std::slice::from_mut(&mut a))
        .expect("write_adb");
    assert_eq!(a.adb_block_count, 2);
    let adb = fs.read(&VfFile::from_path(&adbf), 0, 2048).unwrap();
    assert_eq!(&adb[0..8], &100u64.to_be_bytes(), "ADBN block 0");
    assert_eq!(&adb[1024..1032], &101u64.to_be_bytes(), "ADBN block 1");
    assert_eq!(&adb[8..11], b"PAT", "pattern block 0");
    assert_eq!(&adb[1032..1035], b"PAT", "pattern block 1");

    // cp_recursive copies the tree.
    let cpsrc = format!("{}/cpsrc", dir);
    let cpdst = format!("{}/cpdst", dir);
    fs.ensure_dir(Path::new(&format!("{}/inner", cpsrc)), 0o755)
        .unwrap();
    let srcfile = format!("{}/inner/data.txt", cpsrc);
    let mut w = WriteOp::from_path(&srcfile, VfOffset::At(0), b"xyz".to_vec());
    w.creation = true;
    fs.writev(&[w]).unwrap();
    if symlinks {
        fs.symlink(
            Path::new("data.txt"),
            Path::new(&format!("{}/inner/link", cpsrc)),
        )
        .unwrap();
    }
    fs.cp_recursive(Path::new(&cpsrc), Path::new(&cpdst), true, false)
        .expect("cp_recursive");
    assert!(
        fs.exists(Path::new(&format!("{}/inner/data.txt", cpdst)))
            .unwrap()
    );
    if symlinks {
        assert_eq!(
            fs.lstat(Path::new(&format!("{}/inner/link", cpdst)))
                .unwrap()
                .ftype,
            VfType::Symlink,
            "cp_recursive(symlinks=true) recreates the link"
        );
        let cpflat = format!("{}/cpflat", dir);
        fs.cp_recursive(Path::new(&cpsrc), Path::new(&cpflat), false, false)
            .expect("cp_recursive no symlinks");
        assert_eq!(
            fs.lstat(Path::new(&format!("{}/inner/link", cpflat)))
                .unwrap()
                .ftype,
            VfType::Regular,
            "cp_recursive(symlinks=false) copies through the link"
        );
        assert_eq!(
            fs.read(
                &VfFile::from_os_path(Path::new(&format!("{}/inner/link", cpflat))),
                0,
                3
            )
            .unwrap(),
            b"xyz"
        );
    }

    // "." and ".." path components resolve identically in both backends.
    let base_name = std::path::Path::new(&dir)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        fs.stat(Path::new(&format!("{}/./renamed.txt", dir)))
            .unwrap()
            .size,
        fs.stat(Path::new(&renamed)).unwrap().size,
        "dot component"
    );
    assert_eq!(
        fs.stat(Path::new(&format!("{}/../{}/renamed.txt", dir, base_name)))
            .unwrap()
            .size,
        fs.stat(Path::new(&renamed)).unwrap().size,
        "dotdot component stays inside the tree"
    );

    // chdir onto a regular file is refused (NOTDIR) by both backends.
    assert_eq!(
        fs.chdir(Path::new(&renamed)).unwrap_err().err_no(),
        ERR_NOTDIR,
        "chdir to a file"
    );

    // O_APPEND writes always go to the end of the file.
    let app = format!("{}/append.txt", dir);
    let afd = fs
        .open(
            Path::new(&app),
            libc::O_CREAT | libc::O_RDWR | libc::O_APPEND,
            0o644,
        )
        .expect("open append");
    fs.writev(&[WriteOp::new(afd.clone(), VfOffset::At(0), b"ab".to_vec())])
        .unwrap();
    fs.writev(&[WriteOp::new(afd.clone(), VfOffset::At(0), b"cd".to_vec())])
        .unwrap();
    let got = fs.read(&VfFile::from_path(&app), 0, 8).unwrap();
    assert_eq!(got, b"abcd", "O_APPEND appends regardless of offset");
    // The reported offsets and tracked position reflect the real append
    // positions, not the requested (ignored) offsets.
    let w0 = fs
        .writev(&[WriteOp::new(afd.clone(), VfOffset::At(0), b"e".to_vec())])
        .unwrap();
    assert_eq!(w0[0].offset, 4, "append write reports the real offset");
    assert_eq!(fs.fseek(&afd, 0, SeekFrom::Cur).unwrap(), 5);
    fs.close(&afd).unwrap();
    assert_eq!(fs.read(&VfFile::from_path(&app), 0, 8).unwrap(), b"abcde");

    // dupv follows a symlink source and copies the target's data.
    if symlinks {
        let dup_link = format!("{}/dup_link", dir);
        let dup_copy = format!("{}/dup_copy", dir);
        let dup_target = format!("{}/dup_target", dir);
        fs.writev(&[
            WriteOp::at(VfFile::from_path(&dup_target), 0, b"linkdata".to_vec()).with_creation(),
        ])
        .unwrap();
        let rel_name = std::path::Path::new(&dup_target)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        fs.symlink(Path::new(&rel_name), Path::new(&dup_link))
            .unwrap();
        fs.dupv(&[ExtentPair::new(&dup_link, 0, &dup_copy, 0, None)])
            .unwrap();
        let st = fs.stat(Path::new(&dup_copy)).unwrap();
        assert_eq!(st.ftype, VfType::Regular, "dupv copies target data");
        assert_eq!(st.size, 8);
        assert_eq!(
            fs.read(&VfFile::from_path(&dup_copy), 0, 8).unwrap(),
            b"linkdata"
        );

        // lsetattrsv refuses symlinks instead of pretending to set them.
        let lsa = VfAttrs {
            file: VfFile::from_path(&dup_link),
            masks: AttrMask::MODE,
            mode: 0o600,
            ..VfAttrs::default()
        };
        assert_eq!(
            fs.lsetattrsv(&[lsa]).unwrap_err().err_no(),
            VF_ERR_UNSUPPORTED,
            "lsetattrsv on a symlink"
        );
    }

    // Closing an already-closed descriptor reports EBADF on both backends.
    assert_eq!(
        fs.close(&tf).unwrap_err().err_no(),
        ERR_EBADF,
        "close of a closed descriptor"
    );

    // open_by_path with an Abs base and a relative-looking path is
    // root-relative in both backends.
    let abs_rel = format!("{}/absrel.txt", base.trim_start_matches('/'));
    let f2 = fs
        .open_by_path(
            VfPathBase::Abs,
            Path::new(&abs_rel),
            libc::O_CREAT | libc::O_RDWR,
            0o644,
        )
        .expect("open_by_path Abs relative");
    fs.writev(&[WriteOp::new(f2.clone(), VfOffset::At(0), b"ar".to_vec())])
        .unwrap();
    fs.close(&f2).unwrap();
    assert_eq!(
        fs.read(
            &VfFile::from_os_path(Path::new(&format!("/{}", abs_rel))),
            0,
            2
        )
        .unwrap(),
        b"ar"
    );

    // Writing through `Current(None)` (the cwd itself) is not a file op.
    assert_eq!(
        fs.writev(&[WriteOp::at(VfFile::cwd(), 0, b"x".to_vec()).with_creation()])
            .unwrap_err()
            .err_no(),
        ERR_ISDIR,
        "write to Current(None)"
    );

    // walk returns pre-order with the sort callback applied to every
    // directory (subdirectories visited in the order the caller lists them).
    let wroot = format!("/{}/wroot", dir.trim_start_matches('/'));
    fs.ensure_dir(Path::new(&wroot), 0o755).unwrap();
    for (sub, file) in [("b", "f1"), ("a", "f2"), ("a", "f3")] {
        let subp = format!("{}/{}", wroot, sub);
        fs.ensure_dir(Path::new(&subp), 0o755).unwrap();
        fs.writev(&[WriteOp::at(
            VfFile::from_os_path(Path::new(&format!("{}/{}", subp, file))),
            0,
            b"x".to_vec(),
        )
        .with_creation()])
            .unwrap();
    }
    let mut sort = |_dir: &Path, attrs: &mut Vec<VfAttrs>| {
        attrs.sort_by(|a, b| {
            a.file
                .path()
                .unwrap()
                .file_name()
                .cmp(&b.file.path().unwrap().file_name())
        });
    };
    let tree = fs
        .walk(Path::new(&wroot), AttrMask::stat(), &mut sort)
        .unwrap();
    let order: Vec<PathBuf> = tree.iter().map(|w| w.path.clone()).collect();
    assert_eq!(
        order,
        vec![
            PathBuf::from(&wroot),
            PathBuf::from(format!("{}/a", wroot)),
            PathBuf::from(format!("{}/b", wroot)),
        ],
        "walk pre-order follows the sorted subdirectory order"
    );
    for w in &tree {
        let names: Vec<_> = w
            .entries
            .iter()
            .map(|e| e.file.path().unwrap().file_name().unwrap().to_owned())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "entries are sorted in {}", w.path.display());
    }

    // chdir / getcwd.
    fs.chdir(Path::new(&sub)).expect("chdir");
    assert_eq!(fs.getcwd().file_name(), Some(Path::new("sub").as_os_str()));

    // rm recursive removes the whole tree.
    fs.rm(&[Path::new(&dir)], true).expect("rm recursive");
    assert!(!fs.exists(Path::new(&dir)).unwrap());
}
