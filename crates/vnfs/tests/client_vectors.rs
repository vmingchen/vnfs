#![cfg(all(feature = "auto", target_os = "linux"))]
use vnfs::{Attributes, AttrsOptions, OpenFlags, OpenOp, Vfsi, VfsiExt, WriteOp};

fn writes<C: Vfsi>(fs: &C) {
    let files = fs
        .vopen(&[
            OpenOp::new("/a", OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE),
            OpenOp::new("/b", OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE),
        ])
        .unwrap();
    let payload = b"payload";
    let op = WriteOp::at(&files[0], 3, payload);
    assert!(std::ptr::eq(op.file(), &files[0]));
    assert_eq!(op.data().as_ptr(), payload.as_ptr());
    assert_eq!(op.offset(), 3);
    let results = fs
        .vwrite(&[op, WriteOp::at(&files[1], 0, b"abc")], Default::default())
        .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|r| (r.offset, r.written))
            .collect::<Vec<_>>(),
        [(3, 7), (0, 3)]
    );
    assert_eq!(
        std::io::Seek::stream_position(&mut fs.file_io(&files[0])).unwrap(),
        0
    );
    fs.vwrite(
        &[
            WriteOp::at(&files[1], 0, b"first"),
            WriteOp::at(&files[1], 2, b"XX"),
        ],
        vnfs::WriteOptions::new().write_all(true),
    )
    .unwrap();
    assert_eq!(fs.read_files(&["/b"]).unwrap(), [b"fiXXt".to_vec()]);
    fs.vwrite(
        &[WriteOp::at(&files[0], 0, b"")],
        vnfs::WriteOptions::new().write_all(true),
    )
    .unwrap();
    fs.close_files(files).unwrap();
}

#[test]
fn portable_writes_on_mounted_and_auto() {
    for auto in [false, true] {
        let root = tempfile::tempdir().unwrap();
        if auto {
            writes(&vnfs::Auto::new(root.path()).unwrap());
        } else {
            writes(&vnfs::Mounted::new(root.path()).unwrap());
        }
    }
}

#[test]
fn complete_writes_reject_the_entire_invalid_batch_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let fs = vnfs::Mounted::new(root.path()).unwrap();
    let other = vnfs::Mounted::new(root.path()).unwrap();
    let file = fs.create("/a").unwrap();
    fs.vwrite(
        &[WriteOp::at(&file, 0, b"keep")],
        vnfs::WriteOptions::new().write_all(true),
    )
    .unwrap();
    let foreign = other.create("/b").unwrap();
    let error = fs
        .vwrite(
            &[
                WriteOp::at(&file, 0, b"bad!"),
                WriteOp::at(&foreign, 0, b""),
            ],
            vnfs::WriteOptions::new().write_all(true),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    let error = fs
        .vwrite(
            &[
                WriteOp::at(&file, 0, b"bad!"),
                WriteOp::at(&file, u64::MAX, b"xx"),
            ],
            vnfs::WriteOptions::new().write_all(true),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(fs.read_files(&["/a"]).unwrap(), [b"keep".to_vec()]);
    assert_eq!(error.operation(), Some("vwrite_native"));
    assert_eq!(error.path(), Some(std::path::Path::new("/a")));
    assert_eq!(error.err_no(), libc::EOVERFLOW as u32);
    let mut closed = fs.create("/closed").unwrap();
    closed.try_close().unwrap();
    assert_eq!(
        fs.vwrite(
            &[WriteOp::at(&file, 0, b"bad!"), WriteOp::at(&closed, 0, b"")],
            vnfs::WriteOptions::new().write_all(true)
        )
        .unwrap_err()
        .index(),
        Some(1)
    );
    assert_eq!(fs.read_files(&["/a"]).unwrap(), [b"keep".to_vec()]);
}

fn attrs_query<C: Vfsi>(fs: &C) {
    assert!(
        fs.attrs_with_options("/link", AttrsOptions::new())
            .unwrap()
            .is_file()
    );
    assert!(
        fs.attrs_with_options("/link", AttrsOptions::new().follow_symlinks(false))
            .unwrap()
            .is_symlink()
    );
    let scalar = fs
        .attrs_with_options("/a", AttrsOptions::new().fields(Attributes::FILEID))
        .unwrap();
    assert!(scalar.file_id().is_some());
    assert_eq!(scalar.len(), None);
    let follow = fs
        .vgetattrs(&["/link", "/a"], vnfs::AttrsOptions::new())
        .unwrap();
    assert!(follow.iter().all(|m| m.is_file() && m.len() == Some(3)));
    let links = fs
        .vgetattrs(
            &["/a", "/link", "/dangling"],
            AttrsOptions::new().follow_symlinks(false),
        )
        .unwrap();
    assert!(links[0].is_file());
    assert!(links[1].is_symlink());
    assert!(links[2].is_symlink());
    assert!(fs.attrs("/link").unwrap().is_file());
    assert!(fs.symlink_attrs("/link").unwrap().is_symlink());
    assert!(fs.attrs("/dangling").is_err());
    let partial = fs
        .vgetattrs(&["/a"], AttrsOptions::new().fields(Attributes::FILEID))
        .unwrap();
    assert!(partial[0].file_id().is_some());
    assert_eq!(
        partial[0].len(),
        None,
        "unrequested size must remain absent/default"
    );
    assert_eq!(
        fs.vgetattrs(&["/a", "/missing"], vnfs::AttrsOptions::new())
            .unwrap_err()
            .index(),
        Some(1)
    );
    assert_eq!(
        fs.vgetattrs(&["/a", "/dangling"], vnfs::AttrsOptions::new())
            .unwrap_err()
            .index(),
        Some(1)
    );
    assert!(
        fs.vgetattrs::<&str>(&[], AttrsOptions::default())
            .unwrap()
            .is_empty()
    );
    assert!(
        fs.attrs_with_options(
            "/link",
            vnfs::AttrsOptions::new()
                .fields(Attributes::MODE)
                .follow_symlinks(false)
        )
        .unwrap()
        .is_symlink()
    );
}

#[test]
fn consolidated_metadata_fields_and_symlinks_on_mounted_and_auto() {
    for auto in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), b"abc").unwrap();
        std::os::unix::fs::symlink("a", root.path().join("link")).unwrap();
        std::os::unix::fs::symlink("missing", root.path().join("dangling")).unwrap();
        if auto {
            attrs_query(&vnfs::Auto::new(root.path()).unwrap());
        } else {
            attrs_query(&vnfs::Mounted::new(root.path()).unwrap());
        }
    }
}
