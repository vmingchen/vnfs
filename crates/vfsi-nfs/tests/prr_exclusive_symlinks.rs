//! Exclusive creation must inspect the final directory entry itself.

use std::path::Path;
use vfsi_nfs::NfsVecFs;
use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::{FileOperand, VfType};

fn connect() -> Option<NfsVecFs> {
    let server = match std::env::var("VFSI_NFS_SERVER") {
        Ok(server) => server,
        Err(_) => {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return None;
        }
    };
    let client = match std::env::var("VFSI_NFS_MINOR").as_deref() {
        Ok("1") => NfsVecFs::connect_minor(&server, 1),
        Ok("2") => NfsVecFs::connect_minor(&server, 2),
        Ok(value) => panic!("unsupported VFSI_NFS_MINOR={value}"),
        Err(_) => NfsVecFs::connect(&server),
    };
    Some(client.expect("connect to configured NFS server"))
}

fn reject_final_symlink(target_exists: bool, looping: bool) {
    let Some(mut fs) = connect() else { return };
    let root = format!(
        "/prr-exclusive-link-{}-{target_exists}-{looping}",
        std::process::id()
    );
    let link = format!("{root}/link");
    let target = format!("{root}/target");
    fs.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
    if target_exists {
        let file = fs
            .open_raw_impl(Path::new(&target), libc::O_CREAT | libc::O_WRONLY, 0o640)
            .unwrap();
        fs.write_raw_impl(&file, 0, b"keep").unwrap();
        fs.close_impl(&file).unwrap();
    }
    fs.vsymlink_impl(
        &[Path::new(if looping { "link" } else { "target" })],
        &[Path::new(&link)],
    )
    .unwrap();
    #[cfg(feature = "test-faults")]
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let result = fs.open_raw_impl(
        Path::new(&link),
        libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_TRUNC,
        0o600,
    );
    #[cfg(feature = "test-faults")]
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    if let Ok(file) = &result {
        fs.close_impl(file).unwrap();
    }
    let link_type = fs.lstat_impl(Path::new(&link)).unwrap().ftype;
    let target_result = fs.stat_impl(Path::new(&target));
    let contents = if target_exists {
        Some(
            fs.read_raw_impl(&FileOperand::from_path(&target), 0, 8)
                .unwrap(),
        )
    } else {
        None
    };
    fs.remove_paths_impl(&[Path::new(&root)], true).unwrap();

    assert_eq!(result.unwrap_err().raw_code(), libc::EEXIST as u32);
    assert_eq!(link_type, VfType::Symlink);
    if target_exists {
        assert_eq!(contents.unwrap(), b"keep");
        assert_eq!(target_result.unwrap().mode & 0o777, 0o640);
    } else {
        assert_eq!(target_result.unwrap_err().raw_code(), libc::ENOENT as u32);
    }
    #[cfg(feature = "test-faults")]
    assert!(
        compounds <= 3,
        "exclusive rejection must not probe/follow the final link: {compounds}"
    );
}

#[test]
fn exclusive_create_rejects_dangling_final_symlink() {
    reject_final_symlink(false, false);
}

#[test]
fn exclusive_create_rejects_looping_final_symlink() {
    reject_final_symlink(false, true);
}

#[test]
fn exclusive_create_preserves_existing_symlink_target() {
    reject_final_symlink(true, false);
}

#[test]
fn ordinary_create_still_follows_dangling_final_symlink() {
    let Some(mut fs) = connect() else { return };
    let root = format!("/prr-ordinary-link-{}", std::process::id());
    let link = format!("{root}/link");
    let target = format!("{root}/target");
    fs.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
    fs.vsymlink_impl(&[Path::new("target")], &[Path::new(&link)])
        .unwrap();
    let file = fs
        .open_raw_impl(Path::new(&link), libc::O_CREAT | libc::O_WRONLY, 0o640)
        .unwrap();
    fs.write_raw_impl(&file, 0, b"created").unwrap();
    fs.close_impl(&file).unwrap();
    assert_eq!(
        fs.read_raw_impl(&FileOperand::from_path(&target), 0, 8)
            .unwrap(),
        b"created"
    );
    assert_eq!(
        fs.lstat_impl(Path::new(&link)).unwrap().ftype,
        VfType::Symlink
    );
    fs.remove_paths_impl(&[Path::new(&root)], true).unwrap();
}

#[test]
fn exclusive_create_still_follows_intermediate_symlink() {
    let Some(mut fs) = connect() else { return };
    let root = format!("/prr-exclusive-parent-link-{}", std::process::id());
    let dir = format!("{root}/dir");
    let link = format!("{root}/link");
    let leaf = format!("{link}/new");
    let target = format!("{dir}/new");
    fs.ensure_dir_impl(Path::new(&dir), 0o755).unwrap();
    fs.vsymlink_impl(&[Path::new("dir")], &[Path::new(&link)])
        .unwrap();
    let file = fs
        .open_raw_impl(
            Path::new(&leaf),
            libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY,
            0o640,
        )
        .unwrap();
    fs.write_raw_impl(&file, 0, b"created").unwrap();
    fs.close_impl(&file).unwrap();
    assert_eq!(
        fs.read_raw_impl(&FileOperand::from_path(&target), 0, 8)
            .unwrap(),
        b"created"
    );
    assert_eq!(
        fs.lstat_impl(Path::new(&link)).unwrap().ftype,
        VfType::Symlink
    );
    fs.remove_paths_impl(&[Path::new(&root)], true).unwrap();
}

#[test]
fn ordinary_create_still_follows_intermediate_symlink() {
    let Some(mut fs) = connect() else { return };
    let root = format!("/prr-ordinary-parent-link-{}", std::process::id());
    let dir = format!("{root}/dir");
    let link = format!("{root}/link");
    let leaf = format!("{link}/new");
    let target = format!("{dir}/new");
    fs.ensure_dir_impl(Path::new(&dir), 0o755).unwrap();
    fs.vsymlink_impl(&[Path::new("dir")], &[Path::new(&link)])
        .unwrap();
    let result = fs.open_raw_impl(Path::new(&leaf), libc::O_CREAT | libc::O_WRONLY, 0o640);
    if let Ok(file) = &result {
        fs.write_raw_impl(file, 0, b"created").unwrap();
        fs.close_impl(file).unwrap();
    }
    let observed = fs.read_raw_impl(&FileOperand::from_path(&target), 0, 8);
    let reopened = if result.is_ok() {
        let file = fs.open_raw_impl(Path::new(&leaf), libc::O_CREAT | libc::O_RDWR, 0o777);
        if let Ok(file) = &file {
            fs.close_impl(file).unwrap();
        }
        Some(file)
    } else {
        None
    };
    let mode = fs
        .stat_impl(Path::new(&target))
        .map(|attrs| attrs.mode & 0o777);
    fs.remove_paths_impl(&[Path::new(&root)], true).unwrap();
    result.expect("ordinary create through a directory symlink");
    reopened
        .unwrap()
        .expect("ordinary create can reopen an existing target through a directory symlink");
    assert_eq!(observed.unwrap(), b"created");
    assert_eq!(mode.unwrap(), 0o640);
}

fn grouped_remove_error(prefix: bool, earlier_in_group: bool, missing_parent: bool) {
    let Some(mut fs) = connect() else { return };
    let root = format!(
        "/prr-remove-index-{}-{prefix}-{earlier_in_group}-{missing_parent}",
        std::process::id()
    );
    let zdir = format!("{root}/zdir");
    let zlink = format!("{root}/zlink");
    let adir = format!("{root}/adir");
    fs.ensure_dir_impl(Path::new(&zdir), 0o755).unwrap();
    fs.vsymlink_impl(&[Path::new("zdir")], &[Path::new(&zlink)])
        .unwrap();
    let create = |fs: &mut NfsVecFs, path: &str| {
        let file = fs
            .open_raw_impl(Path::new(path), libc::O_CREAT | libc::O_WRONLY, 0o600)
            .unwrap();
        fs.close_impl(&file).unwrap();
    };
    create(&mut fs, &format!("{zdir}/good"));
    let mut files = Vec::new();
    if prefix {
        let path = format!("{root}/prefix");
        create(&mut fs, &path);
        files.push(FileOperand::from_path(&path));
    }
    files.push(FileOperand::from_path(&format!("{zlink}/good")));
    if !missing_parent {
        fs.ensure_dir_impl(Path::new(&adir), 0o755).unwrap();
    }
    if earlier_in_group {
        let path = format!("{adir}/good");
        create(&mut fs, &path);
        files.push(FileOperand::from_path(&path));
    }
    files.push(FileOperand::from_path(&format!("{adir}/missing")));
    let expected = files.len() - 1;
    let result = fs.vremove_impl(&files);
    fs.remove_paths_impl(&[Path::new(&root)], true).unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.raw_code(), libc::ENOENT as u32);
    assert_eq!(
        error.index(),
        Some(expected),
        "group-local errors identify the original input"
    );
}

#[test]
fn grouped_remove_reports_original_index_across_parent_groups() {
    grouped_remove_error(false, false, false);
}

#[test]
fn grouped_remove_reports_original_index_within_parent_group() {
    grouped_remove_error(false, true, false);
}

#[test]
fn grouped_remove_reports_original_index_after_completed_prefix() {
    grouped_remove_error(true, true, false);
}

#[test]
fn grouped_remove_parent_resolution_reports_original_index() {
    grouped_remove_error(true, false, true);
}

#[cfg(feature = "test-faults")]
#[test]
fn grouped_remove_keeps_same_parent_names_batched() {
    use std::sync::Arc;
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Some(mut fs) = connect() else { return };
    let root = format!("/prr-remove-batch-{}", std::process::id());
    let dir = format!("{root}/dir");
    let link = format!("{root}/link");
    fs.ensure_dir_impl(Path::new(&dir), 0o755).unwrap();
    fs.vsymlink_impl(&[Path::new("dir")], &[Path::new(&link)])
        .unwrap();
    let files: Vec<_> = (0..16)
        .map(|index| {
            let file = fs
                .open_raw_impl(
                    Path::new(&format!("{dir}/file-{index}")),
                    libc::O_CREAT | libc::O_WRONLY,
                    0o600,
                )
                .unwrap();
            fs.close_impl(&file).unwrap();
            FileOperand::from_path(&format!("{link}/file-{index}"))
        })
        .collect();
    let script = Arc::new(FaultScript::new([]));
    fs.set_fault_injector(script.clone());
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let result = fs.vremove_impl(&files);
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    let remove_chunks = script
        .visited()
        .iter()
        .filter(|point| matches!(point, OpenFaultPoint::BeforeRemoveChunk { .. }))
        .count();
    fs.remove_paths_impl(&[Path::new(&root)], true).unwrap();
    result.unwrap();
    assert_eq!(
        remove_chunks, 1,
        "same-parent fallback removes remain batched"
    );
    assert!(
        compounds <= 7,
        "fallback should resolve once and remove one batch: {compounds}"
    );
}
