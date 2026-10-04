#![cfg(all(feature = "auto", target_os = "linux"))]
use vnfs::{MetadataFields, MetadataOptions, OpenFlags, OpenRequest, Vfsi, VfsiExt, WriteOp};

fn writes<C: Vfsi>(fs: &C) {
    let mut files = fs
        .vopen(&[
            OpenRequest::new("/a", OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE),
            OpenRequest::new("/b", OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE),
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
    assert_eq!(std::io::Seek::stream_position(&mut files[0]).unwrap(), 0);
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

fn metadata<C: Vfsi>(fs: &C) {
    assert!(
        fs.metadata_with_options("/link", MetadataOptions::new())
            .unwrap()
            .is_file()
    );
    assert!(
        fs.metadata_with_options("/link", MetadataOptions::new().follow_symlinks(false))
            .unwrap()
            .is_symlink()
    );
    let scalar = fs
        .metadata_with_options("/a", MetadataOptions::new().fields(MetadataFields::FILEID))
        .unwrap();
    assert!(scalar.file_id().is_some());
    assert_eq!(scalar.len(), 0);
    let follow = fs
        .vgetattrs(&["/link", "/a"], vnfs::MetadataOptions::new())
        .unwrap();
    assert!(follow.iter().all(|m| m.is_file() && m.len() == 3));
    let links = fs
        .vgetattrs(
            &["/a", "/link", "/dangling"],
            MetadataOptions::new().follow_symlinks(false),
        )
        .unwrap();
    assert!(links[0].is_file());
    assert!(links[1].is_symlink());
    assert!(links[2].is_symlink());
    assert!(fs.metadata("/link").unwrap().is_file());
    assert!(fs.symlink_metadata("/link").unwrap().is_symlink());
    assert!(fs.metadata("/dangling").is_err());
    let partial = fs
        .vgetattrs(
            &["/a"],
            MetadataOptions::new().fields(MetadataFields::FILEID),
        )
        .unwrap();
    assert!(partial[0].file_id().is_some());
    assert_eq!(
        partial[0].len(),
        0,
        "unrequested size must remain absent/default"
    );
    assert_eq!(
        fs.vgetattrs(&["/a", "/missing"], vnfs::MetadataOptions::new())
            .unwrap_err()
            .index(),
        Some(1)
    );
    assert_eq!(
        fs.vgetattrs(&["/a", "/dangling"], vnfs::MetadataOptions::new())
            .unwrap_err()
            .index(),
        Some(1)
    );
    assert!(
        fs.vgetattrs::<&str>(&[], MetadataOptions::default())
            .unwrap()
            .is_empty()
    );
    assert!(
        fs.metadata_with_options(
            "/link",
            vnfs::MetadataOptions::new()
                .fields(MetadataFields::MODE)
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
            metadata(&vnfs::Auto::new(root.path()).unwrap());
        } else {
            metadata(&vnfs::Mounted::new(root.path()).unwrap());
        }
    }
}
