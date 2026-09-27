//! Public-package integration coverage for the NFS backend.

use std::path::Path;
#[cfg(feature = "test-faults")]
use std::{path::PathBuf, sync::Arc};

#[cfg(feature = "test-faults")]
use vfsi_core::VfError;
#[cfg(feature = "test-faults")]
use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
use vfsi_nfs::NfsVecFs;
#[cfg(feature = "test-faults")]
use vfsi_nfs::client::{FileRef, NfsClient, OpenCreate, PathWriteOp};
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

#[cfg(feature = "test-faults")]
#[test]
fn pre_dispatch_resource_rejection_splits_merged_writes_without_replay() {
    let server = match std::env::var("VFSI_NFS_SERVER") {
        Ok(server) => server,
        Err(_) => {
            assert!(!required(), "VFSI_NFS_SERVER is required");
            return;
        }
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .ok()
        .map(|value| value.parse::<u32>().expect("NFS minor version"))
        .unwrap_or(2);
    let mut client = NfsClient::connect_minor(&server, minor).expect("connect NFS client");
    let root = client.root().clone();
    let dirname = format!("vfsi-nfs-resource-fault-{}", std::process::id());
    let dir = client
        .mkdir(&root, &dirname)
        .expect("create test directory");
    let names: Vec<Vec<u8>> = (0..24)
        .map(|index| format!("file-{index}").into_bytes())
        .collect();
    let writes: Vec<PathWriteOp> = names
        .iter()
        .enumerate()
        .map(|(index, name)| PathWriteOp {
            file: FileRef::Path(
                format!("{dirname}/{}", String::from_utf8_lossy(name)).into_bytes(),
            ),
            offset: 0,
            data: vec![index as u8; 16],
            create: true,
            truncate: true,
            stateid: None,
        })
        .collect();

    client.inject_resource_rejection_once(b"writev1");
    let result = client
        .writev_path_compound(&writes, true)
        .expect("retry rejected write compound");
    assert!(!client.resource_rejection_pending());
    assert_eq!(result.failed, None);
    assert_eq!(result.close_failed, None);
    assert_eq!(result.counts, vec![Some(16); writes.len()]);
    for (index, name) in names.iter().enumerate() {
        let (fh, stateid) = client
            .open(
                &dir,
                name,
                nfsv41_sys::OPEN4_SHARE_ACCESS_READ,
                OpenCreate::NoCreate,
            )
            .expect("open written file");
        let (data, _) = client
            .read(&fh, &stateid, 0, 32)
            .expect("read written file");
        assert_eq!(data, vec![index as u8; 16]);
        client.close(&fh, &stateid).expect("close written file");
    }
    client.remove_many(&dir, &names).expect("remove test files");
    client
        .remove(&root, &dirname)
        .expect("remove test directory");
}

#[cfg(feature = "test-faults")]
#[test]
fn failed_strict_open_quarantines_an_unconfirmed_cleanup_handle() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping NFS integration test: VFSI_NFS_SERVER not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-nfs-cleanup-fault-{}", std::process::id()));
    let _ = fs.rm(&[root.as_path()], true);
    fs.mkdir(root.as_path(), 0o755).unwrap();
    let paths = [root.join("one"), root.join("two")];
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let script = Arc::new(FaultScript::new([
        (
            OpenFaultPoint::BeforeRegister { index: 1 },
            VfError::transport(None, "injected registration failure"),
        ),
        (
            OpenFaultPoint::BeforeCloseDispatch { index: 0 },
            VfError::transport(None, "injected cleanup close failure"),
        ),
    ]));
    fs.set_fault_injector(script.clone());
    assert!(
        fs.openv(&refs, &[libc::O_CREAT | libc::O_RDWR; 2], &[0o600; 2],)
            .unwrap_err()
            .is_transport()
    );
    assert!(script.is_consumed());
    assert_eq!(fs.test_open_handle_count(), 0);
    assert_eq!(fs.test_deferred_descriptor_close_count(), 1);
    let next_path = root.join("next");
    let next = fs
        .openv(
            &[next_path.as_path()],
            &[libc::O_CREAT | libc::O_RDWR],
            &[0o600],
        )
        .unwrap();
    assert_eq!(fs.test_deferred_descriptor_close_count(), 0);
    fs.closev(&next).unwrap();
    fs.rm(&[root.as_path()], true).unwrap();
}
