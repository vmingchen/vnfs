use vnfs::{FilesystemStats, Target, Vfsi, VfsiExt};

fn check_stats(stats: &FilesystemStats) {
    let total = stats.total_bytes.expect("total capacity");
    let free = stats.free_bytes.expect("free capacity");
    let available = stats.available_bytes.expect("available capacity");
    assert!(total > 0);
    assert!(available <= free && free <= total);
    assert!(stats.max_name_len.unwrap() > 0);
}

pub fn check<F: Vfsi>(fs: &F, other: &F, directory: &str) {
    assert!(fs.vstatfs::<&str>(&[]).unwrap().is_empty());
    let paths: Vec<_> = (0..64).map(|i| format!("{directory}/stats-{i}")).collect();
    fs.write_files(
        &paths
            .iter()
            .map(|p| (p, b"data".as_slice()))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let stats = fs.vstatfs(&paths).unwrap();
    assert_eq!(stats.len(), paths.len());
    for value in &stats {
        check_stats(value);
    }
    let single = fs.statfs(&paths[0]).unwrap();
    assert_eq!(single.total_bytes, stats[0].total_bytes);
    let requests: Vec<_> = paths
        .iter()
        .map(|p| vnfs::OpenOp::new(p, vnfs::OpenFlags::READ))
        .collect();
    let mut files = fs.vopen(&requests).unwrap();
    let renamed = format!("{directory}/renamed");
    fs.rename(&paths[0], &renamed).unwrap();
    let mixed = [
        Target::File(&files[0]),
        Target::Path(std::path::Path::new(&paths[1])),
    ];
    let stats = fs.vstatfs(&mixed).unwrap();
    assert_eq!(stats[0].total_bytes, stats[1].total_bytes);
    // A renamed handle must not reopen its missing original name.
    check_stats(&fs.statfs(Target::File(&files[0])).unwrap());
    let error = fs.vstatfs(&[&paths[1], &paths[0]]).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::ENOENT as u32);
    let error = other.vstatfs(&[Target::File(&files[1])]).unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.err_no(), libc::EINVAL as u32);
    fs.vclose(&mut files[..1]).unwrap();
    let error = fs
        .vstatfs(&[Target::File(&files[1]), Target::File(&files[0])])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EBADF as u32);
    fs.vclose(&mut files[1..]).unwrap();
}
