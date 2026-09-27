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
use vfsi_nfs::client::{FileRef, NfsClient, OpenCreate, PathOpenOp, PathWriteOp, ReadOp, WriteOp};
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
fn split_path_write_truncates_only_before_the_first_chunk() {
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
    let name = format!("vfsi-nfs-split-truncate-{}", std::process::id());
    let data = vec![b'x'; 64 * 1024];

    // The limit includes framing, so the payload must span two compounds.
    client.set_max_compound_bytes(64 * 1024);
    let outcome = client
        .writev_path_compound(
            &[PathWriteOp {
                file: FileRef::Path(name.as_bytes().to_vec()),
                offset: 0,
                data: data.clone(),
                create: true,
                truncate: true,
                stateid: None,
            }],
            true,
        )
        .expect("write split path payload");
    assert_eq!(outcome.failed, None);
    assert_eq!(outcome.counts, vec![Some(data.len() as u32)]);
    client.set_max_compound_bytes(0);

    let (fh, stateid) = client
        .open(
            &root,
            name.as_bytes(),
            nfsv41_sys::OPEN4_SHARE_ACCESS_READ,
            OpenCreate::NoCreate,
        )
        .expect("open written file");
    let (actual, _) = client
        .read(&fh, &stateid, 0, data.len() as u32)
        .expect("read written file");
    assert_eq!(actual, data);
    client.close(&fh, &stateid).expect("close written file");
    client.remove(&root, &name).expect("remove test file");
}

#[cfg(feature = "test-faults")]
#[test]
fn variable_depth_paths_repack_before_exceeding_compound_limit() {
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
    let dirname = format!("vfsi-nfs-deep-path-{}", std::process::id());
    let mut parent = client
        .mkdir(&root, &dirname)
        .expect("create root directory");
    let top = parent.clone();
    let mut dirs = vec![(root.clone(), dirname.clone())];
    let mut deep_path = dirname.clone();
    for index in 0..24 {
        let name = format!("d{index}");
        let child = client.mkdir(&parent, &name).expect("create path component");
        dirs.push((parent, name.clone()));
        parent = child;
        deep_path.push('/');
        deep_path.push_str(&name);
    }

    // The shallow and deep OPENs each fit in 32 operations, but the
    // combined variable-length path resolution does not. The builder must
    // repack before dispatch instead of failing its hard-cap check.
    client.max_ops = 32;
    let outcome = client
        .openv_path_compound(&[
            PathOpenOp {
                path: format!("{dirname}/shallow").into_bytes(),
                access: nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
                create: OpenCreate::Unchecked,
                mode: None,
                truncate: false,
            },
            PathOpenOp {
                path: format!("{deep_path}/deep").into_bytes(),
                access: nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
                create: OpenCreate::Unchecked,
                mode: None,
                truncate: false,
            },
        ])
        .expect("split variable-length paths");
    assert_eq!(outcome.failed, None);
    for (fh, stateid) in outcome.opened.into_iter().map(Option::unwrap) {
        client.close(&fh, &stateid).expect("close opened file");
    }
    client.remove(&top, "shallow").expect("remove shallow file");
    client.remove(&parent, "deep").expect("remove deep file");
    for (dir, name) in dirs.into_iter().rev() {
        client.remove(&dir, &name).expect("remove test directory");
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn encoded_request_limit_rejects_oversize_write_before_dispatch() {
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
    let name = format!("vfsi-nfs-byte-limit-{}", std::process::id());
    let (fh, stateid) = client
        .open(
            &root,
            name.as_bytes(),
            nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
            OpenCreate::Unchecked,
        )
        .expect("create test file");
    client.set_max_compound_bytes(1400);
    let error = client
        .write(&fh, &stateid, 0, &[7; 512])
        .expect_err("oversize encoded request must be rejected locally");
    assert_eq!(error.status, nfsv41_sys::nfsstat4_NFS4ERR_REQ_TOO_BIG);
    client.set_max_compound_bytes(0);
    let (data, _) = client.read(&fh, &stateid, 0, 512).expect("read test file");
    assert!(data.is_empty(), "rejected WRITE must not reach the server");
    let oversized_read = client
        .readv(&[ReadOp {
            fh: fh.clone(),
            stateid,
            offset: 0,
            count: u32::MAX,
        }])
        .expect_err("unbounded READ reply must be rejected locally");
    assert_eq!(
        oversized_read.status,
        nfsv41_sys::nfsstat4_NFS4ERR_REP_TOO_BIG
    );
    client.close(&fh, &stateid).expect("close test file");
    client.remove(&root, &name).expect("remove test file");
}

#[cfg(feature = "test-faults")]
#[test]
fn writev_rebuilds_batch_before_exceeding_request_bytes() {
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
    let name = format!("vfsi-nfs-byte-batch-{}", std::process::id());
    let (fh, stateid) = client
        .open(
            &root,
            name.as_bytes(),
            nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
            OpenCreate::Unchecked,
        )
        .expect("create test file");
    let block = 512 * 1024;
    let ops: Vec<WriteOp> = (0..8)
        .map(|index| WriteOp {
            fh: fh.clone(),
            stateid,
            offset: (index * block) as u64,
            data: vec![index as u8; block],
        })
        .collect();
    let written = client
        .writev(&ops)
        .expect("repack encoded byte-heavy batch");
    assert_eq!(written.len(), ops.len());
    assert!(written.iter().all(|(count, _)| *count as usize == block));
    let reads: Vec<ReadOp> = (0..8)
        .map(|index| ReadOp {
            fh: fh.clone(),
            stateid,
            offset: (index * block) as u64,
            count: block as u32,
        })
        .collect();
    let read_results = client.readv(&reads).expect("bound aggregate reply bytes");
    for (index, (data, _)) in read_results.iter().enumerate() {
        assert_eq!(data.len(), block);
        assert!(data.iter().all(|byte| *byte == index as u8));
    }
    let (data, _) = client
        .read(&fh, &stateid, (7 * block) as u64, 16)
        .expect("read last block");
    assert_eq!(data, vec![7; 16]);
    client.close(&fh, &stateid).expect("close test file");
    client.remove(&root, &name).expect("remove test file");
}

#[cfg(feature = "test-faults")]
#[test]
fn openv_repacks_variable_length_names_for_encoded_byte_limit() {
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
    let dirname = format!("vfsi-nfs-byte-path-{}", std::process::id());
    let dir = client
        .mkdir(&root, &dirname)
        .expect("create test directory");
    let names: Vec<String> = (0..4)
        .map(|index| format!("{index}-{}", "x".repeat(236)))
        .collect();
    let opens: Vec<PathOpenOp> = names
        .iter()
        .map(|name| PathOpenOp {
            path: format!("{dirname}/{name}").into_bytes(),
            access: nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
            create: OpenCreate::Unchecked,
            mode: None,
            truncate: false,
        })
        .collect();
    client.set_max_compound_bytes(2100);
    let outcome = client
        .openv_path_compound(&opens)
        .expect("repack encoded variable-length paths");
    assert_eq!(outcome.failed, None);
    client.set_max_compound_bytes(0);
    for (fh, stateid) in outcome.opened.into_iter().map(Option::unwrap) {
        client.close(&fh, &stateid).expect("close test file");
    }
    for name in names {
        client.remove(&dir, &name).expect("remove test file");
    }
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
