use vnfs::{MetadataFields, MetadataOptions, MetadataUpdate, Permissions, Vfsi, VfsiExt};

pub fn check_many(fs: &impl Vfsi, directory: &str) {
    fs.vsetattrs::<&str>(&[], true).unwrap();
    let paths: Vec<_> = (0..96).map(|i| format!("{directory}/file-{i}")).collect();
    let writes: Vec<_> = paths
        .iter()
        .map(|path| (path, b"abcdefgh".as_slice()))
        .collect();
    fs.write_files(&writes).unwrap();
    let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let updates: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            (
                path,
                MetadataUpdate::new()
                    .permissions(Permissions::from_mode(0o600 | ((i % 8) as u32)))
                    .len(if i % 2 == 0 { 3 } else { 100 + i as u64 })
                    .modified(modified),
            )
        })
        .collect();
    fs.vsetattrs(&updates, true).unwrap();
    let options = MetadataOptions::new()
        .fields(MetadataFields::MODE | MetadataFields::SIZE | MetadataFields::MTIME);
    let metadata = fs.vgetattrs(&paths, options).unwrap();
    assert_eq!(metadata.len(), paths.len());
    for (i, item) in metadata.iter().enumerate() {
        assert_eq!(item.permissions().mode() & 0o7777, 0o600 | ((i % 8) as u32));
        assert_eq!(item.len(), if i % 2 == 0 { 3 } else { 100 + i as u64 });
        assert_eq!(item.modified(), Some(modified));
    }
    // Size-only mutations preserve permissions and retain zero as a valid size.
    let sizes: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| (path, MetadataUpdate::new().len(i as u64)))
        .collect();
    fs.vsetattrs(&sizes, false).unwrap();
    let metadata = fs.vgetattrs(&paths, options).unwrap();
    for (i, item) in metadata.iter().enumerate() {
        assert_eq!(item.len(), i as u64);
        assert_eq!(item.permissions().mode() & 0o7777, 0o600 | ((i % 8) as u32));
    }
    let missing = format!("{directory}/missing");
    let error = fs
        .vsetattrs(
            &[
                (&paths[0], MetadataUpdate::new().len(7)),
                (&missing, MetadataUpdate::new().len(7)),
                (&paths[1], MetadataUpdate::new().len(7)),
            ],
            true,
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::ENOENT as u32);
}
