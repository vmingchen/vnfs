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
    let mut w = WriteOp::from_path(&f, 0, payload.clone());
    w.creation = true;
    let wr = &fs.writev(&[w]).expect("writev")[0];
    assert_eq!(wr.written, payload.len());

    let r = &fs
        .readv(&[ReadOp::from_path(&f, 0, payload.len())])
        .expect("readv")[0];
    assert_eq!(r.data, payload);
    assert!(!r.eof);

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
    fs.writev(&[WriteOp::from_fd(tf.fd().unwrap(), 0, b"hello".to_vec())])
        .expect("writev fd");
    assert_eq!(fs.fseek(&mut tf.clone(), 0, SeekFrom::Set).unwrap(), 0);
    let r = &fs
        .readv(&[ReadOp::from_fd(tf.fd().unwrap(), VF_OFFSET_CUR, 5)])
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
    let mut w = WriteOp::from_path(&srcfile, 0, b"xyz".to_vec());
    w.creation = true;
    fs.writev(&[w]).unwrap();
    fs.cp_recursive(&cpsrc, &cpdst, true, false)
        .expect("cp_recursive");
    assert!(fs.exists(&format!("{}/inner/data.txt", cpdst)).unwrap());

    // chdir / getcwd.
    fs.chdir(&sub).expect("chdir");
    assert!(fs.getcwd().ends_with("/sub"));

    // rm recursive removes the whole tree.
    fs.rm(&[dir.as_str()], true).expect("rm recursive");
    assert!(!fs.exists(&dir).unwrap());
}
