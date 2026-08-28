//! A shared test suite run against any [`VecFs`] implementation, proving both
//! backends (NFS and the `std::fs` dummy) behave identically.

use vnfs::VecFs;
use vnfs::vecfs::*;

/// Run a broad set of vectorized-filesystem assertions against `fs`, using
/// paths under `base` (which must be unique per caller).
pub fn run_suite(fs: &mut impl VecFs, base: &str) {
    let dir = format!("{}/suite", base);
    fs.ensure_dir(&dir, 0o755).expect("ensure_dir");
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
    let st = fs.stat(&f).expect("stat");
    assert_eq!(st.size, payload.len() as u64);
    assert!(st.fileid != 0);
    assert!(fs.exists(&f).unwrap());
    assert_eq!(fs.file_type(&f).unwrap(), VfType::Regular);

    // setattrs: truncate to 5 bytes, then mode.
    fs.setattrsv(&[VfAttrs {
        file: VfFile::from_path(&f),
        masks: AttrMask::SIZE,
        size: 5,
        ..VfAttrs::default()
    }])
    .expect("setattrsv size");
    assert_eq!(fs.stat(&f).unwrap().size, 5);

    // mkdir.
    let sub = format!("{}/sub", dir);
    fs.mkdir(&sub, 0o750).expect("mkdir");
    assert_eq!(fs.stat(&sub).unwrap().ftype, VfType::Directory);
    assert_eq!(fs.stat(&sub).unwrap().mode & 0o777, 0o750);

    // open / descriptor write / fseek / descriptor read.
    let tf = fs.open(&f, libc::O_RDWR, 0).expect("open");
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

    // symlink / readlink.
    let link = format!("{}/ln", dir);
    fs.symlink(&f, &link).expect("symlink");
    assert_eq!(fs.readlink(&link).unwrap(), f.as_bytes());

    // stat follows symlinks; lstat does not. (Relative target so both the
    // NFS and std::fs backends can resolve it.)
    let sbase = format!("{}/lnstat", dir);
    let rel = std::path::Path::new(&f)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    fs.symlink(&rel, &sbase).expect("symlink lnstat");
    assert_eq!(
        fs.stat(&sbase).unwrap().ftype,
        VfType::Regular,
        "stat follows"
    );
    assert_eq!(
        fs.lstat(&sbase).unwrap().ftype,
        VfType::Symlink,
        "lstat stays"
    );
    assert_eq!(
        fs.stat(&sbase).unwrap().size,
        fs.stat(&f).unwrap().size,
        "stat resolves to the target"
    );

    // hardlink.
    let hard = format!("{}/hard", dir);
    fs.hardlinkv(&[f.as_str()], &[hard.as_str()])
        .expect("hardlinkv");
    assert_eq!(fs.stat(&f).unwrap().fileid, fs.stat(&hard).unwrap().fileid);

    // rename.
    let renamed = format!("{}/renamed.txt", dir);
    fs.renamev(&[(VfFile::from_path(&f), VfFile::from_path(&renamed))])
        .expect("renamev");
    assert!(fs.exists(&renamed).unwrap());
    assert!(!fs.exists(&f).unwrap());

    // openv / closev.
    let more = format!("{}/more", dir);
    fs.ensure_dir(&more, 0o755).unwrap();
    let paths = [
        format!("{}/a", more),
        format!("{}/b", more),
        format!("{}/c", more),
    ];
    let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    let files = fs
        .openv(&refs, &[libc::O_CREAT | libc::O_RDWR; 3], &[0o644; 3])
        .expect("openv");
    fs.closev(&files).expect("closev");

    // listdir (non-recursive) finds entries.
    let entries = fs
        .listdir(&dir, AttrMask::default(), 0, false)
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
    let mut cb = |_: &VfAttrs, _: &str| {
        seen += 1;
        true
    };
    fs.listdirv(&[dir.as_str()], AttrMask::default(), 0, false, &mut cb)
        .expect("listdirv");
    assert!(seen >= 2);

    // dupv extent copy (whole file).
    let copy = format!("{}/copy.txt", dir);
    fs.dupv(&[ExtentPair::new(&renamed, 0, &copy, 0, u64::MAX)])
        .expect("dupv");
    assert_eq!(
        fs.stat(&copy).unwrap().size,
        fs.stat(&renamed).unwrap().size
    );

    // write_adb: two blocks with ADBN at block offset 0.
    let adbf = format!("{}/adb.bin", dir);
    let mut a = Adb::blocknum_only(&adbf, 0, 1024, 2, 0, 100);
    fs.write_adb(std::slice::from_mut(&mut a))
        .expect("write_adb");
    assert_eq!(a.adb_block_count, 2);

    // cp_recursive copies the tree.
    let cpsrc = format!("{}/cpsrc", dir);
    let cpdst = format!("{}/cpdst", dir);
    fs.ensure_dir(&format!("{}/inner", cpsrc), 0o755).unwrap();
    let srcfile = format!("{}/inner/data.txt", cpsrc);
    let mut w = WriteOp::from_path(&srcfile, VfOffset::At(0), b"xyz".to_vec());
    w.creation = true;
    fs.writev(&[w]).unwrap();
    fs.cp_recursive(&cpsrc, &cpdst, true, false)
        .expect("cp_recursive");
    assert!(fs.exists(&format!("{}/inner/data.txt", cpdst)).unwrap());

    // "." and ".." path components resolve identically in both backends.
    let base_name = std::path::Path::new(&dir)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        fs.stat(&format!("{}/./renamed.txt", dir)).unwrap().size,
        fs.stat(&renamed).unwrap().size,
        "dot component"
    );
    assert_eq!(
        fs.stat(&format!("{}/../{}/renamed.txt", dir, base_name))
            .unwrap()
            .size,
        fs.stat(&renamed).unwrap().size,
        "dotdot component stays inside the tree"
    );

    // chdir onto a regular file is refused (NOTDIR) by both backends.
    assert_eq!(
        fs.chdir(&renamed).unwrap_err().err_no(),
        ERR_NOTDIR,
        "chdir to a file"
    );

    // O_APPEND writes always go to the end of the file.
    let app = format!("{}/append.txt", dir);
    let afd = fs
        .open(&app, libc::O_CREAT | libc::O_RDWR | libc::O_APPEND, 0o644)
        .expect("open append");
    fs.writev(&[WriteOp::new(afd.clone(), VfOffset::At(0), b"ab".to_vec())])
        .unwrap();
    fs.writev(&[WriteOp::new(afd.clone(), VfOffset::At(0), b"cd".to_vec())])
        .unwrap();
    fs.close(&afd).unwrap();
    let got = fs.read(&VfFile::from_path(&app), 0, 8).unwrap();
    assert_eq!(got, b"abcd", "O_APPEND appends regardless of offset");

    // dupv follows a symlink source and copies the target's data.
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
    fs.symlink(&rel_name, &dup_link).unwrap();
    fs.dupv(&[ExtentPair::new(&dup_link, 0, &dup_copy, 0, u64::MAX)])
        .unwrap();
    let st = fs.stat(&dup_copy).unwrap();
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
            &abs_rel,
            libc::O_CREAT | libc::O_RDWR,
            0o644,
        )
        .expect("open_by_path Abs relative");
    fs.writev(&[WriteOp::new(f2.clone(), VfOffset::At(0), b"ar".to_vec())])
        .unwrap();
    fs.close(&f2).unwrap();
    assert_eq!(
        fs.read(&VfFile::from_path(&format!("/{}", abs_rel)), 0, 2)
            .unwrap(),
        b"ar"
    );

    // Writing through `Current(None)` (the cwd itself) is not a file op.
    assert_eq!(
        fs.writev(&[WriteOp::at(VfFile::current(None), 0, b"x".to_vec()).with_creation()])
            .unwrap_err()
            .err_no(),
        ERR_ISDIR,
        "write to Current(None)"
    );

    // chdir / getcwd.
    fs.chdir(&sub).expect("chdir");
    assert!(fs.getcwd().ends_with("/sub"));

    // rm recursive removes the whole tree.
    fs.rm(&[dir.as_str()], true).expect("rm recursive");
    assert!(!fs.exists(&dir).unwrap());
}
