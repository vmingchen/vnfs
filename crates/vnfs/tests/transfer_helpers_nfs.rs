#![cfg(feature = "nfs")]

use vnfs::{directory::*, error::*, files::*, helpers::*, nfs::*};

#[test]
fn transfers_and_statistics_on_nfsv41_and_nfsv42() {
    let Ok(host) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let export = std::env::var("VFSI_NFS_EXPORT").unwrap_or_else(|_| "/".into());
    let versions = match std::env::var("VFSI_NFS_MINOR").as_deref() {
        Ok("1") => vec![NfsVersion::V4_1],
        Ok("2") => vec![NfsVersion::V4_2],
        Err(std::env::VarError::NotPresent) => vec![NfsVersion::V4_1, NfsVersion::V4_2],
        v => panic!("invalid minor: {v:?}"),
    };
    for version in versions {
        let fs = Nfs::builder(&host)
            .root(&export)
            .version(version)
            .connect()
            .unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = format!("/transfer-{}-{nonce}", std::process::id());
        TreeBuilder::new()
            .add_directory("source/nested")
            .create(&fs, &root)
            .unwrap();
        let operation = (|| -> Result<()> {
            let src = format!("{root}/source");
            TreeBuilder::new()
                .add_file("a", vec![19; 97])
                .add_file("b", b"second")
                .create(&fs, format!("{src}/nested/files"))
                .map_err(vfsi_core::IndexedError::into_cause)?;
            let destination = format!("{root}/no-replace");
            let error = fs
                .renames_all(&[(&src, &destination)], RenameMode::NoReplace)
                .unwrap_err();
            assert_eq!(error.cause().kind(), std::io::ErrorKind::Unsupported);
            // Default conflict-safe copying uses bounded descriptor vectors.
            let s = copy_tree(
                &fs,
                &src,
                format!("{root}/descriptor"),
                TransferOptions::new().with_chunk_size(
                    std::num::NonZeroUsize::new(16).expect("nonzero tuning value"),
                ),
            )?;
            assert_eq!(s.files_copied, 2);
            assert_eq!(s.bytes_copied, Some(103));
            let s = tree_stats(&fs, format!("{root}/descriptor"), ListDirOptions::new())?;
            assert_eq!((s.files, s.directories, s.file_bytes), (2, 3, 103));
            let path = format!("{root}/descriptor/nested/files/a");
            let data = fs
                .read_batch([ReadOp::whole(&path)], ReadOptions::new())
                .map_err(vfsi_core::IndexedError::into_cause)?;
            assert_eq!(data[0].data().unwrap(), vec![19; 97]);
            // Replace selects the server COPY path on v4.2, with v4.1 fallback.
            let s = copy_tree(
                &fs,
                &src,
                format!("{root}/native"),
                TransferOptions::new().with_existing(Existing::Replace),
            )?;
            assert_eq!(s.files_copied, 2);
            assert_eq!(s.bytes_copied, None);
            // Re-copy into existing directories exercises replacement reconciliation.
            copy_tree(
                &fs,
                &src,
                format!("{root}/native"),
                TransferOptions::new().with_existing(Existing::Replace),
            )?;
            let path = format!("{root}/native/nested/files/b");
            let data = fs
                .read_batch([ReadOp::whole(&path)], ReadOptions::new())
                .map_err(vfsi_core::IndexedError::into_cause)?;
            assert_eq!(data[0].data().unwrap(), b"second");
            let s = move_items(
                &fs,
                &[format!("{root}/descriptor")],
                format!("{root}/moved"),
                TransferOptions::new(),
            )
            .map_err(vfsi_core::IndexedError::into_cause)?;
            assert_eq!(s.roots_removed, 1);
            assert_eq!(
                fs.attrs(format!("{root}/descriptor")).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
            let mut builder = TreeBuilder::new();
            for i in 0..16 {
                builder = builder.add_file(format!("f-{i}"), format!("payload-{i}"));
            }
            let batchsrc = format!("{root}/batch-source");
            builder
                .create(&fs, &batchsrc)
                .map_err(vfsi_core::IndexedError::into_cause)?;
            vnfs::diagnostics::take_and_reset();
            copy_tree(
                &fs,
                &batchsrc,
                format!("{root}/batch-copy"),
                TransferOptions::new(),
            )?;
            let batched = vnfs::diagnostics::take_and_reset();
            copy_tree(
                &fs,
                &batchsrc,
                format!("{root}/serial-copy"),
                TransferOptions::new()
                    .with_batch_size(std::num::NonZeroUsize::new(1).expect("nonzero tuning value")),
            )?;
            let serial = vnfs::diagnostics::take_and_reset();
            eprintln!(
                "{version:?}: transfer compounds batched={}, serial={}",
                batched.compounds, serial.compounds
            );
            assert!(
                batched.compounds < serial.compounds,
                "copy vectorization regressed"
            );
            let payload = format!("{root}/base/folder/same/folder/same/payload");
            fs.create_dir_all(format!("{root}/base/folder/same/folder/same"))?;
            fs.write(&payload, b"retain")?;
            let link = format!("{root}/shortcut");
            fs.symlink("missing/../base/folder", &link)?;
            let aliased = format!("{link}/same");
            let contents = TransferOptions::new().with_layout(CopyLayout::Contents);
            let overlap =
                move_items(&fs, &[&aliased], format!("{root}/base"), contents).unwrap_err();
            assert_eq!(overlap.index(), Some(0));
            assert_eq!(overlap.cause().kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!(fs.read(&payload)?, b"retain");
            let moved = move_items(&fs, &[&aliased], format!("{root}/separate"), contents)
                .map_err(vfsi_core::IndexedError::into_cause)?;
            assert_eq!(moved.roots_removed, 1);
            assert_eq!(
                fs.read(format!("{root}/separate/folder/same/payload"))?,
                b"retain"
            );
            fs.create_dir_all(format!("{root}/new-parent"))?;
            fs.symlink("missing/../new-parent", format!("{root}/destination-link"))?;
            copy_tree(
                &fs,
                format!("{root}/separate"),
                format!("{root}/destination-link/new"),
                TransferOptions::new(),
            )?;
            assert_eq!(
                fs.read(format!("{root}/new-parent/new/folder/same/payload"))?,
                b"retain"
            );

            Ok(())
        })();
        let cleanup = fs.remove_dir_all(&root);
        operation.unwrap();
        cleanup.unwrap();
    }
}

#[cfg(all(feature = "auto", target_os = "linux"))]
#[test]
#[ignore = "requires two NFS subtree mounts and an NFS bind fixture"]
fn mounted_nfs_transfer_recognizes_the_same_server_subtrees() {
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;
    use vnfs::{mounted::Auto, posix::Posix};

    let root = PathBuf::from(std::env::var_os("VFSI_TRANSFER_NFS_ROOT").expect("mount fixture"));
    let device = |name| std::fs::metadata(root.join(name)).unwrap().dev();
    assert_ne!(device("whole"), device("separate"));
    assert_eq!(device("whole"), device("bound"));

    fn check(fs: &impl Vfsi, disjoint: &str) {
        let options = TransferOptions::new()
            .with_layout(CopyLayout::Contents)
            .with_existing(Existing::Replace)
            .with_max_depth(DepthLimit::new(0).unwrap());
        for alias in ["/separate", "/bound"] {
            let error = copy_items(fs, &["/whole/tree"], alias, options).unwrap_err();
            assert_eq!(error.index(), Some(0));
            assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!(fs.read("/whole/tree/file").unwrap(), b"top source");
            assert_eq!(
                fs.read("/whole/tree/nested/file").unwrap(),
                b"nested source"
            );
        }

        let source = format!("/whole/{disjoint}");
        let target = format!("/bound/{disjoint}");
        let copied = copy_tree(fs, &source, &target, TransferOptions::new()).unwrap();
        assert_eq!(copied.files_copied, 1);
        assert_eq!(fs.read(format!("{source}/file")).unwrap(), b"disjoint");
        assert_eq!(fs.read(format!("{target}/file")).unwrap(), b"disjoint");
    }

    check(&Posix::new(&root).unwrap(), "disjoint-posix");
    let auto = Auto::new(&root).unwrap();
    check(&auto, "disjoint-auto");
    auto.create_dir("/bound/disjoint-auto/fresh").unwrap();
    let created = "/bound/disjoint-auto/fresh/file";
    auto.write(created, b"ordinary create").unwrap();
    assert_eq!(auto.read(created).unwrap(), b"ordinary create");
}
