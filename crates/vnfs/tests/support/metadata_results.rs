use std::path::Path;
use vnfs::directory::{Attributes, AttrsOptions};
use vnfs::error::FailureState;
use vnfs::files::{FileHandle, OpenFlags, OpenOp, Target, Vfsi, VfsiExt};

pub fn check<F: Vfsi>(fs: &F, other: &F, directory: &str) {
    let one = format!("{directory}/one");
    let two = format!("{directory}/two");
    let missing = format!("{directory}/missing");
    let link = format!("{directory}/link");
    fs.write_files(&[(&one, b"one".as_slice()), (&two, b"second")])
        .unwrap();
    let options = AttrsOptions::new().with_fields(Attributes::SIZE);
    let paths = [&one, &missing, &two];
    let results = fs.vgetattrs(&paths, options).unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].as_ref().unwrap().len(), Some(3));
    assert_eq!(results[2].as_ref().unwrap().len(), Some(6));
    let error = results[1].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::Failed);
    assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
    assert_eq!(error.cause().path(), Some(Path::new(&missing)));
    assert_eq!(error.cause().index(), None);
    assert_eq!(
        fs.getattrs_all(&paths, options).unwrap_err().index(),
        Some(1)
    );

    fs.symlink("one", &link).unwrap();
    let results = fs
        .vgetattrs(
            &[&link, &missing, &one],
            options.with_follow_final_symlink(false),
        )
        .unwrap();
    assert!(results[0].as_ref().unwrap().is_symlink());
    assert!(results[1].is_err());
    assert_eq!(results[2].as_ref().unwrap().len(), Some(3));
    let results = fs.vgetattrs(&[&link, &missing, &two], options).unwrap();
    assert_eq!(results[0].as_ref().unwrap().len(), Some(3));
    assert_eq!(results[2].as_ref().unwrap().len(), Some(6));

    let mut files = fs
        .open_batch(&[
            OpenOp::new(&one, OpenFlags::READ),
            OpenOp::new(&two, OpenFlags::READ),
        ])
        .unwrap();
    fs.rename(&one, format!("{directory}/moved")).unwrap();
    let results = fs
        .vgetattrs(
            &[
                Target::File(&files[0]),
                Target::Path(Path::new(&missing)),
                Target::File(&files[1]),
            ],
            options.with_follow_final_symlink(false),
        )
        .unwrap();
    assert_eq!(results[0].as_ref().unwrap().len(), Some(3));
    assert_eq!(results[2].as_ref().unwrap().len(), Some(6));
    assert!(results[1].is_err());

    let foreign = other.open(&two).unwrap();
    fs.close_batch(&mut files[..1]).unwrap();
    let results = fs
        .vgetattrs(
            &[
                Target::File(&files[0]),
                Target::File(&files[1]),
                Target::Path(Path::new(&two)),
                Target::File(&foreign),
            ],
            options,
        )
        .unwrap();
    assert_eq!(results.len(), 4);
    for (index, code) in [(0, libc::EBADF), (3, libc::EINVAL)] {
        let error = results[index].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::NotAttempted);
        assert_eq!(error.cause().raw_code(), code as u32);
        assert_eq!(error.cause().index(), None);
    }
    assert_eq!(results[1].as_ref().unwrap().len(), Some(6));
    assert_eq!(results[2].as_ref().unwrap().len(), Some(6));
    fs.close_batch(&mut files[1..]).unwrap();
    foreign.close().unwrap();
    assert!(fs.vgetattrs::<&str>(&[], options).unwrap().is_empty());
}
