#![cfg(all(feature = "nfs", feature = "auto", target_os = "linux"))]

#[allow(dead_code)]
#[path = "../examples/bulk_files.rs"]
mod bulk_files;
#[allow(dead_code)]
#[path = "../examples/directories.rs"]
mod directories;
#[allow(dead_code)]
#[path = "../examples/open_handles.rs"]
mod open_handles;
#[allow(dead_code)]
#[path = "../examples/stream_file.rs"]
mod stream_file;

use vnfs::VfsiExt;
use vnfs::{Mounted, Result, Vfsi};

#[test]
fn grouped_namespaces_use_the_same_application_types() {
    let _: vnfs::nfs::NfsBuilder = vnfs::Nfs::builder("server");
    let _: vnfs::files::ResourceLimits = vnfs::ResourceLimits::default();
    let _: vnfs::directory::ListDirOptions = vnfs::ListDirOptions::new();
    let _: vnfs::error::Result<()> = Ok::<(), vnfs::Error>(());
    let _: vnfs::mounted::AutoRoute = vnfs::AutoRoute::Mounted;
}

#[test]
fn bulk_roundtrip_cleanup_and_existing_directory_protection() -> Result<()> {
    let root = tempfile::tempdir().unwrap();
    let fs = Mounted::new(root.path())?;
    assert_eq!(
        bulk_files::run(&fs, "/fresh")?,
        [b"hello".to_vec(), b"world".to_vec()]
    );
    assert!(!root.path().join("fresh").exists());
    fs.create_dir("/existing")?;
    fs.write("/existing/precious", b"keep")?;
    assert!(bulk_files::run(&fs, "/existing").is_err());
    assert_eq!(
        fs.vread(
            [vnfs::ReadOp::whole("/existing/precious")],
            vnfs::ReadOptions::default()
        )?[0]
            .data()
            .unwrap(),
        b"keep"
    );
    let bounded = fs
        .clone()
        .with_limits(vnfs::ResourceLimits::new().max_read_bytes(3));
    // Fail after creating/writing the owned directory, then still clean it up.
    assert!(bulk_files::run(&bounded, "/failed-read").is_err());
    assert!(!root.path().join("failed-read").exists());
    Ok(())
}

#[test]
fn handle_ranges_preserve_order_for_empty_short_and_large_files() -> Result<()> {
    let root = tempfile::tempdir().unwrap();
    let fs = Mounted::new(root.path())?;
    fs.write_files(&[
        ("/empty", &[][..]),
        ("/short", b"hello"),
        ("/large", &vec![42; 9000]),
    ])?;
    let paths = ["/large", "/empty", "/short"].map(str::to_owned);
    assert_eq!(
        open_handles::run(&fs, &paths)?,
        [vec![42; 4096], vec![], b"hello".to_vec()]
    );
    assert!(open_handles::run(&fs, &["/missing".to_owned()]).is_err());
    Ok(())
}

#[test]
fn stream_multiple_chunks_and_empty_file() -> Result<()> {
    let root = tempfile::tempdir().unwrap();
    let fs = Mounted::new(root.path())?;
    let bytes = 2 * 1024 * 1024 + 7;
    fs.write("/large", &vec![37; bytes])?;
    fs.write("/empty", &[])?;
    assert_eq!(stream_file::run(&fs, "/large")?, bytes as u64);
    assert_eq!(stream_file::run(&fs, "/empty")?, 0);
    assert!(stream_file::run(&fs, "/missing").is_err());
    Ok(())
}

#[test]
fn directory_batches_and_no_follow_walk() -> Result<()> {
    let root = tempfile::tempdir().unwrap();
    let fs = Mounted::new(root.path())?;
    fs.create_dir_all("/tree/sub")?;
    fs.write("/tree/sub/file", b"data")?;
    fs.create_dir("/outside")?;
    fs.write("/outside/hidden", b"outside")?;
    std::os::unix::fs::symlink(root.path().join("outside"), root.path().join("tree/link")).unwrap();
    assert_eq!(
        directories::run(&fs, &["/tree".into(), "/tree/sub".into()], "/tree")?,
        3
    );
    Ok(())
}

#[test]
#[ignore = "requires VFSI_NFS_SERVER and a writable VFSI_NFS_EXPORT"]
fn canonical_workflows_on_nfsv41_and_nfsv42() -> Result<()> {
    use vnfs::{Nfs, NfsVersion, helpers::TreeBuilder};
    let host = std::env::var("VFSI_NFS_SERVER").expect("set VFSI_NFS_SERVER");
    let export = std::env::var("VFSI_NFS_EXPORT").expect("set VFSI_NFS_EXPORT");
    let versions = match std::env::var("VFSI_NFS_MINOR").as_deref() {
        Ok("1") => vec![NfsVersion::V4_1],
        Ok("2") => vec![NfsVersion::V4_2],
        Err(std::env::VarError::NotPresent) => vec![NfsVersion::V4_1, NfsVersion::V4_2],
        other => panic!("invalid VFSI_NFS_MINOR: {other:?}"),
    };
    for version in versions {
        let fs = Nfs::builder(&host)
            .root(&export)
            .version(version)
            .connect()?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = format!("/canonical-example-{}-{nonce}", std::process::id());
        let bytes = 2 * 1024 * 1024 + 7;
        let tree = TreeBuilder::new()
            .add_empty_file("empty")
            .add_file("large", vec![37; bytes])
            .add_file("sub/file", b"hello")
            .create(&fs, &root)?;
        let result = (|| -> Result<()> {
            assert_eq!(
                bulk_files::run(&fs, &format!("{root}/bulk"))?,
                [b"hello".to_vec(), b"world".to_vec()]
            );
            let paths = [
                format!("{root}/large"),
                format!("{root}/empty"),
                format!("{root}/sub/file"),
            ];
            assert_eq!(
                open_handles::run(&fs, &paths)?,
                [vec![37; 4096], vec![], b"hello".to_vec()]
            );
            assert_eq!(stream_file::run(&fs, &paths[0])?, bytes as u64);
            let file = fs.open(&paths[0])?;
            let mut buffer = [0; 4];
            let mixed = fs.vread(
                [
                    vnfs::ReadOp::whole(&paths[2]),
                    vnfs::ReadOp::range(&file, 10, 3),
                    vnfs::ReadOp::whole(&paths[1]),
                    vnfs::ReadOp::into(&file, 20, &mut buffer),
                ],
                vnfs::ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(12)),
            )?;
            assert_eq!(mixed[0].data().unwrap(), b"hello");
            assert_eq!(mixed[1].offset(), 10);
            assert_eq!(mixed[1].data().unwrap(), [37; 3]);
            assert!(mixed[2].data().as_ref().unwrap().is_empty() && mixed[2].eof());
            assert_eq!(mixed[3].data(), None);
            assert_eq!(mixed[3].read(), 4);
            assert_eq!(buffer, [37; 4]);
            file.close()?;
            assert_eq!(
                directories::run(&fs, &[root.clone(), format!("{root}/sub")], &root)?,
                4
            );
            Ok(())
        })();
        let cleanup = fs.remove_dir_all(tree.root());
        result?;
        cleanup?;
    }
    Ok(())
}
