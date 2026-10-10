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
                .create(&fs, format!("{src}/nested/files"))?;
            let destination = format!("{root}/no-replace");
            let error = fs
                .vrename(&[(&src, &destination)], RenameOptions::NoReplace)
                .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
            // Default conflict-safe copying uses bounded descriptor vectors.
            let s = copy_tree(
                &fs,
                &src,
                format!("{root}/descriptor"),
                CopyOptions::new().chunk_bytes(16),
            )?;
            assert_eq!(s.files_copied, 2);
            assert_eq!(s.bytes_copied, Some(103));
            let s = tree_stats(&fs, format!("{root}/descriptor"), ListDirOptions::new())?;
            assert_eq!((s.files, s.directories, s.file_bytes), (2, 3, 103));
            let path = format!("{root}/descriptor/nested/files/a");
            let data = fs.vread([ReadOp::whole(&path)], ReadOptions::new())?;
            assert_eq!(data[0].data().unwrap(), vec![19; 97]);
            // Replace selects the server COPY path on v4.2, with v4.1 fallback.
            let s = copy_tree(
                &fs,
                &src,
                format!("{root}/native"),
                CopyOptions::new().existing(Existing::Replace),
            )?;
            assert_eq!(s.files_copied, 2);
            assert_eq!(s.bytes_copied, None);
            // Re-copy into existing directories exercises replacement reconciliation.
            copy_tree(
                &fs,
                &src,
                format!("{root}/native"),
                CopyOptions::new().existing(Existing::Replace),
            )?;
            let path = format!("{root}/native/nested/files/b");
            let data = fs.vread([ReadOp::whole(&path)], ReadOptions::new())?;
            assert_eq!(data[0].data().unwrap(), b"second");
            let s = move_items(
                &fs,
                &[format!("{root}/descriptor")],
                format!("{root}/moved"),
                CopyOptions::new(),
            )?;
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
            builder.create(&fs, &batchsrc)?;
            vnfs::diagnostics::take_and_reset();
            copy_tree(
                &fs,
                &batchsrc,
                format!("{root}/batch-copy"),
                CopyOptions::new(),
            )?;
            let batched = vnfs::diagnostics::take_and_reset();
            copy_tree(
                &fs,
                &batchsrc,
                format!("{root}/serial-copy"),
                CopyOptions::new().batch_size(1),
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
            Ok(())
        })();
        let cleanup = fs.remove_dir_all(&root);
        operation.unwrap();
        cleanup.unwrap();
    }
}
