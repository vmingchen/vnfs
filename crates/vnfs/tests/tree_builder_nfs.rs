#![cfg(feature = "nfs")]

use vnfs::{Nfs, NfsVersion, helpers::TreeBuilder};

/// Optional live coverage; set VFSI_NFS_SERVER and VFSI_NFS_EXPORT. CI can
/// require it with VFSI_NFS_REQUIRED=1, matching the backend integration suite.
#[test]
fn tree_creation_batches_compounds_on_nfsv41_and_nfsv42() {
    let Ok(host) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let export = std::env::var("VFSI_NFS_EXPORT").unwrap_or_else(|_| "/".into());
    let versions = match std::env::var("VFSI_NFS_MINOR").as_deref() {
        Ok("1") => vec![NfsVersion::V4_1],
        Ok("2") => vec![NfsVersion::V4_2],
        Err(std::env::VarError::NotPresent) => vec![NfsVersion::V4_1, NfsVersion::V4_2],
        value => panic!("invalid VFSI_NFS_MINOR: {value:?}"),
    };
    for version in versions {
        let client = Nfs::builder(&host)
            .root(&export)
            .version(version)
            .connect()
            .unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = format!("/tree-helper-{}-{nonce}", std::process::id());
        let mut builder = TreeBuilder::new();
        for index in 0..16 {
            builder = builder.add_file(format!("dir-{index}/file"), format!("payload-{index}"));
        }
        vnfs::diagnostics::take_and_reset();
        let created = builder.create(&client, &root);
        let stats = vnfs::diagnostics::take_and_reset();
        let tree = created.unwrap();
        eprintln!(
            "{version:?}: {} compounds for 16 directories + 16 files",
            stats.compounds
        );
        let paths: Vec<_> = (0..16).map(|i| format!("{root}/dir-{i}/file")).collect();
        let requests: Vec<_> = paths.iter().map(vnfs::ReadOp::whole).collect();
        let contents = client.readv(requests).map(|results| {
            results
                .into_iter()
                .map(|result| result.data.unwrap())
                .collect::<Vec<_>>()
        });
        // Clean the successfully created fixture before assertions, including
        // when verification failed. Never remove a root we failed to create.
        let cleanup = client.remove_dir_all(tree.root());
        assert_eq!(
            contents.unwrap(),
            (0..16)
                .map(|i| format!("payload-{i}").into_bytes())
                .collect::<Vec<_>>()
        );
        cleanup.unwrap();
        let serial_root = format!("{root}-serial");
        let mut serial = TreeBuilder::new().batch_size(1);
        for index in 0..16 {
            serial = serial.add_file(format!("dir-{index}/file"), format!("payload-{index}"));
        }
        vnfs::diagnostics::take_and_reset();
        let tree = serial.create(&client, &serial_root).unwrap();
        let serial_stats = vnfs::diagnostics::take_and_reset();
        client.remove_dir_all(tree.root()).unwrap();
        eprintln!(
            "{version:?}: {} compounds with batch_size=1",
            serial_stats.compounds
        );
        assert!(
            stats.compounds < serial_stats.compounds,
            "vectorization regressed: vector={stats:?}, serial={serial_stats:?}"
        );
    }
}
