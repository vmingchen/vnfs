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
    let mut w = VfIoVec::from_path(&f, 0, payload.len(), payload.clone());
    w.is_creation = true;
    fs.writev(std::slice::from_mut(&mut w)).expect("writev");
    assert_eq!(w.length, payload.len());

    let mut r = VfIoVec::from_path(&f, 0, payload.len(), Vec::new());
    fs.readv(std::slice::from_mut(&mut r)).expect("readv");
    assert_eq!(r.data, payload);
    assert!(!r.is_failure);

    // stat / exists / file_type.
    let st = fs.stat(&f).expect("stat");
    assert_eq!(st.size, payload.len() as u64);
    assert!(st.fileid != 0);
    assert!(fs.exists(&f));
    assert_eq!(fs.file_type(&f).unwrap(), VfType::Regular);

    // setattrs: truncate to 5 bytes, then mode.
    fs.setattrsv(&[VfAttrs {
        file: VfFile::from_path(&f),
        masks: AttrMask {
            has_size: true,
            ..AttrMask::default()
        },
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
    let mut w = VfIoVec::from_fd(tf.fd().unwrap(), 0, 5, b"hello".to_vec());
    fs.writev(std::slice::from_mut(&mut w)).expect("writev fd");
    assert_eq!(fs.fseek(&mut tf.clone(), 0, libc::SEEK_SET).unwrap(), 0);
    let mut r = VfIoVec::from_fd(tf.fd().unwrap(), VF_OFFSET_CUR, 5, Vec::new());
    fs.readv(std::slice::from_mut(&mut r)).expect("readv fd");
    assert_eq!(r.data, b"hello");
    fs.close(&tf).expect("close");

    // symlink / readlink.
    let link = format!("{}/ln", dir);
    fs.symlink(&f, &link).expect("symlink");
    assert_eq!(fs.readlink(&link).unwrap(), f.as_bytes());

    // hardlink.
    let hard = format!("{}/hard", dir);
    fs.hardlinkv(&[f.as_str()], &[hard.as_str()])
        .expect("hardlinkv");
    assert_eq!(fs.stat(&f).unwrap().fileid, fs.stat(&hard).unwrap().fileid);

    // rename.
    let renamed = format!("{}/renamed.txt", dir);
    fs.renamev(&[(VfFile::from_path(&f), VfFile::from_path(&renamed))])
        .expect("renamev");
    assert!(fs.exists(&renamed));
    assert!(!fs.exists(&f));

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
    let mut w = VfIoVec::from_path(&srcfile, 0, 3, b"xyz".to_vec());
    w.is_creation = true;
    fs.writev(std::slice::from_mut(&mut w)).unwrap();
    fs.cp_recursive(&cpsrc, &cpdst, true, false)
        .expect("cp_recursive");
    assert!(fs.exists(&format!("{}/inner/data.txt", cpdst)));

    // chdir / getcwd.
    fs.chdir(&sub).expect("chdir");
    assert!(fs.getcwd().ends_with("/sub"));

    // rm recursive removes the whole tree.
    fs.rm(&[dir.as_str()], true).expect("rm recursive");
    assert!(!fs.exists(&dir));
}
