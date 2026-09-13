//! Public-package integration coverage for the NFS backend.

use std::path::Path;

use vfsi_nfs::NfsVecFs;
use vfsi_sync::{VecFs, test_support};

fn required() -> bool {
    std::env::var("VFSI_NFS_REQUIRED").as_deref() == Ok("1")
}

fn connect() -> Option<NfsVecFs> {
    let server = match std::env::var("VFSI_NFS_SERVER") {
        Ok(server) => server,
        Err(_) => {
            assert!(
                !required(),
                "VFSI_NFS_SERVER is required in this integration job"
            );
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

#[test]
fn shared_contract_through_the_published_crate() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping NFS integration test: VFSI_NFS_SERVER not set");
        return;
    };
    let root = format!("/vfsi-nfs-contract-{}", std::process::id());
    let _ = fs.rm(&[Path::new(&root)], true);
    test_support::run_suite(&mut fs, &root);
    fs.rm(&[Path::new(&root)], true)
        .expect("remove NFS contract root");
}
