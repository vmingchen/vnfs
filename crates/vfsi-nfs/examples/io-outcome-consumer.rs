//! Public Vfsi consumer for ci/nfs_outcome_fixture.py; no fault API in production.
use std::io::{Read, Write};
use vfsi_core::api::{
    OpenFlags, OpenOp, ReadOp, ReadOptions, Vfsi, VfsiExt, WriteOp, WriteOptions,
};
use vfsi_core::{FailureState, IoProgress, StatusCode};
use vfsi_nfs::NfsVecFs;
use vfsi_sync::FsClient;

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 3);
    let minor = args[2].parse().unwrap();
    let mode = std::env::var("VFSI_OUTCOME_FAULT").unwrap();
    let large = matches!(mode.as_str(), "later" | "uncached-later");
    let independent = mode == "uncached-later";
    let path = format!("/vfsi-rust-outcomes-{}", std::process::id());
    let fs = FsClient::new(NfsVecFs::connect_minor(&args[1], minor).unwrap());
    let file = fs
        .open_with(OpenOp::new(
            &path,
            OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE_NEW,
        ))
        .unwrap();
    let mut retired = fs.open(&path).unwrap();
    fs.vclose(std::slice::from_mut(&mut retired))
        .unwrap()
        .pop()
        .unwrap()
        .unwrap();
    let data = if large {
        vec![b'a'; 16 * 1024 * 1024]
    } else {
        b"abcdefghijklmnop".to_vec()
    };
    let mut requests = vec![WriteOp::at(&file, 0, &data)];
    if large {
        requests.push(WriteOp::at(&file, 32 * 1024 * 1024, b"b"));
    }
    requests.push(WriteOp::at(&retired, 0, b"bad"));
    println!("READY");
    std::io::stdout().flush().unwrap();
    let mut go = [0];
    std::io::stdin().read_exact(&mut go).unwrap();
    assert_eq!(go, [b'\n']);
    let outcomes = fs.vwrite(&requests, WriteOptions::new()).unwrap();
    assert_eq!(outcomes.len(), requests.len());
    if mode == "short" {
        assert_eq!(outcomes[0].as_ref().unwrap().bytes_written, 16);
    } else {
        let error = outcomes[0].as_ref().unwrap_err();
        assert_eq!(
            error.state(),
            if mode == "prefix" {
                FailureState::Failed
            } else {
                FailureState::Unknown
            }
        );
        assert_eq!(
            error.progress(),
            if mode == "prefix" {
                Some(&IoProgress::Write { bytes_written: 4 })
            } else {
                None
            }
        );
        if mode.starts_with("uncached") || mode == "prefix" {
            assert_eq!(
                error.cause().status(),
                Some(StatusCode::Nfs(if mode == "prefix" { 28 } else { 10068 }))
            );
        } else {
            assert!(error.cause().is_transport());
        }
    }
    if large {
        if independent {
            let result = outcomes[1].as_ref().unwrap();
            assert_eq!((result.offset, result.bytes_written), (32 * 1024 * 1024, 1));
        } else {
            let error = outcomes[1].as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::NotAttempted);
            assert!(error.cause().is_transport());
            assert!(error.progress().is_none());
        }
    }
    let rejected = outcomes.last().unwrap().as_ref().unwrap_err();
    assert_eq!(rejected.state(), FailureState::NotAttempted);
    assert!(rejected.progress().is_none());
    print!("OUTCOMES");
    for result in &outcomes {
        let (state, count) = match result {
            Ok(result) => ("success", result.bytes_written),
            Err(error) => {
                let state = match error.state() {
                    FailureState::Unknown => "unknown",
                    FailureState::Failed => "failed",
                    FailureState::NotAttempted => "not_attempted",
                };
                let count = match error.progress() {
                    Some(IoProgress::Write { bytes_written }) => *bytes_written,
                    None => 0,
                    _ => panic!("unexpected progress type"),
                };
                (state, count)
            }
        };
        print!(" {state}:{count}");
    }
    println!();
    drop(requests);
    drop(retired);
    drop(file);
    drop(fs);

    let fresh = FsClient::new(
        NfsVecFs::connect_minor(&std::env::var("VFSI_OUTCOME_VERIFY_SERVER").unwrap(), minor)
            .unwrap(),
    );
    let mut reader = fresh.open(&path).unwrap();
    let result = fresh
        .vread([ReadOp::range(&reader, 0, 32)], ReadOptions::new())
        .unwrap()
        .pop()
        .unwrap()
        .unwrap();
    let expected: &[u8] = if large {
        &[b'a'; 32]
    } else if mode == "prefix" {
        b"abcd"
    } else {
        b"abcdefghijklmnop"
    };
    assert_eq!(result.data().unwrap(), expected);
    if independent {
        let result = fresh
            .vread(
                [ReadOp::range(&reader, 32 * 1024 * 1024, 1)],
                ReadOptions::new(),
            )
            .unwrap()
            .pop()
            .unwrap()
            .unwrap();
        assert_eq!(result.data().unwrap(), b"b");
    }
    fresh
        .vclose(std::slice::from_mut(&mut reader))
        .unwrap()
        .pop()
        .unwrap()
        .unwrap();
    assert!(reader.is_closed());
    fresh.remove_file(&path).unwrap();
    println!("Rust outcome consumer: PASS");
}
