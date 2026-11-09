use vnfs::files::{FilesystemStats, Target, Vfsi, VfsiExt};

fn check_stats(stats: &FilesystemStats) {
    let total = stats.total_bytes().expect("total capacity");
    let free = stats.free_bytes().expect("free capacity");
    let available = stats.available_bytes().expect("available capacity");
    assert!(total > 0);
    assert!(available <= free && free <= total);
    assert!(stats.max_name_len().unwrap() > 0);
}

pub fn check<F: Vfsi>(fs: &F, other: &F, directory: &str) {
    assert!(fs.statfs_all::<&str>(&[]).unwrap().is_empty());
    let paths: Vec<_> = (0..64).map(|i| format!("{directory}/stats-{i}")).collect();
    fs.write_files(
        &paths
            .iter()
            .map(|p| (p, b"data".as_slice()))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let stats = fs.statfs_all(&paths).unwrap();
    assert_eq!(stats.len(), paths.len());
    for value in &stats {
        check_stats(value);
    }
    let missing = format!("{directory}/missing-statfs-input");
    let outcomes = fs.vstatfs(&[&paths[0], &missing, &paths[1]]).unwrap();
    assert_eq!(outcomes.len(), 3);
    check_stats(outcomes[0].as_ref().unwrap());
    check_stats(outcomes[2].as_ref().unwrap());
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), vnfs::error::FailureState::Failed);
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
    let single = fs.statfs(&paths[0]).unwrap();
    assert_eq!(single.total_bytes(), stats[0].total_bytes());
    let requests: Vec<_> = paths
        .iter()
        .map(|p| vnfs::files::OpenOp::new(p, vnfs::files::OpenFlags::READ))
        .collect();
    let mut files = fs.open_batch(&requests).unwrap();
    let renamed = format!("{directory}/renamed");
    fs.rename(&paths[0], &renamed).unwrap();
    let mixed = [
        Target::File(&files[0]),
        Target::Path(std::path::Path::new(&paths[1])),
    ];
    let stats = fs.statfs_all(&mixed).unwrap();
    assert_eq!(stats[0].total_bytes(), stats[1].total_bytes());
    // A renamed handle must not reopen its missing original name.
    check_stats(&fs.statfs(Target::File(&files[0])).unwrap());
    let error = fs.statfs_all(&[&paths[1], &paths[0]]).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.raw_code(), libc::ENOENT as u32);
    let error = other.statfs_all(&[Target::File(&files[1])]).unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.raw_code(), libc::EINVAL as u32);
    fs.close_batch(&mut files[..1]).unwrap();
    let error = fs
        .statfs_all(&[Target::File(&files[1]), Target::File(&files[0])])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.raw_code(), libc::EBADF as u32);
    let outcomes = fs
        .vstatfs(&[
            Target::File(&files[1]),
            Target::File(&files[0]),
            Target::File(&files[2]),
        ])
        .unwrap();
    assert_eq!(outcomes.len(), 3);
    check_stats(outcomes[0].as_ref().unwrap());
    check_stats(outcomes[2].as_ref().unwrap());
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), vnfs::error::FailureState::NotAttempted);
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().raw_code(), libc::EBADF as u32);
    fs.close_batch(&mut files[1..]).unwrap();
}
