#![cfg(feature = "ffi")]

use std::path::Path;
use vfsi_nfs::NfsVecFs;
use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::{FileOperand, VfOffset};

fn server() -> Option<String> {
    match std::env::var("VFSI_NFS_SERVER") {
        Ok(server) => Some(server),
        Err(_) => {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            None
        }
    }
}
fn minor() -> u32 {
    std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "2".into())
        .parse()
        .unwrap()
}
fn mixed_append(mode: &str) {
    let Some(server) = server() else { return };
    for offset in [VfOffset::Cur, VfOffset::At(0), VfOffset::At(u64::MAX)] {
        for reverse in [false, true] {
            let mut fs = NfsVecFs::connect_minor(&server, minor()).unwrap();
            fs.set_merged_mode(mode);
            let root = format!(
                "/prr-rust11-append-{}-{mode}-{offset:?}-{reverse}",
                std::process::id()
            );
            fs.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
            let first = format!("{root}/first");
            let second = format!("{root}/second");
            for (path, data) in [(&first, b"abc".as_slice()), (&second, b"XYZ")] {
                let file = fs
                    .open_raw_impl(
                        Path::new(path),
                        libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                        0o600,
                    )
                    .unwrap();
                fs.write_raw_impl(&file, 0, data).unwrap();
                fs.close_impl(&file).unwrap();
            }
            let file = fs
                .open_raw_impl(Path::new(&first), libc::O_RDWR | libc::O_APPEND, 0)
                .unwrap();
            let mut requests = vec![
                vfsi_sync::WriteOp::new(file.clone(), offset, b"d".to_vec()),
                vfsi_sync::WriteOp::from_os_path(
                    Path::new(&second),
                    VfOffset::At(0),
                    b"!".to_vec(),
                ),
            ];
            if reverse {
                requests.reverse();
            }
            let results = fs
                .vwrite_impl(&vfsi_sync::test_support::borrow_writes(&requests))
                .unwrap();
            let actual = fs.read_raw_impl(&file, 0, 8).unwrap();
            let other = fs
                .read_raw_impl(&FileOperand::from_path(&second), 0, 8)
                .unwrap();
            fs.close_impl(&file).unwrap();
            fs.remove_paths_impl(&[Path::new(&root)], true).unwrap();
            assert_eq!(
                actual, b"abcd",
                "mode={mode}, offset={offset:?}, reverse={reverse}"
            );
            assert_eq!(other, b"!YZ");
            assert_eq!(results[usize::from(reverse)].offset, 3);
        }
    }
}
#[test]
fn mixed_append_full() {
    mixed_append("full");
}
#[test]
fn mixed_append_openwrite() {
    mixed_append("openwrite");
}
#[test]
fn mixed_append_off_control() {
    mixed_append("off");
}

#[cfg(feature = "test-faults")]
mod short_merged {
    use super::*;
    use nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH;
    use vfsi_nfs::client::{CloseOp, FileRef, NfsClient, OpenCreate, PathReadOp, PathWriteOp};

    fn fixture(
        kind: &str,
        close: bool,
    ) -> Option<(NfsClient, vfsi_nfs::client::NfsFh, Vec<u8>, Vec<u8>)> {
        let server = server()?;
        let mut client = NfsClient::connect_minor(&server, minor()).unwrap();
        let root = client.root().clone();
        let name = format!("prr-rust11-{kind}-{}-{close}", std::process::id());
        let dir = client.mkdir(&root, &name).unwrap();
        let (fh, stateid) = client
            .open(
                &dir,
                b"file",
                OPEN4_SHARE_ACCESS_BOTH,
                OpenCreate::Unchecked,
            )
            .unwrap();
        client.set_max_compound_bytes(4096);
        let per = client.per_op_bytes();
        let data: Vec<u8> = (0..per * 2 + 17)
            .map(|index| (index % 251 + 1) as u8)
            .collect();
        for (index, chunk) in data.chunks(per).enumerate() {
            assert_eq!(
                client
                    .write(&fh, &stateid, (index * per) as u64, chunk)
                    .unwrap()
                    .0 as usize,
                chunk.len()
            );
        }
        client.close(&fh, &stateid).unwrap();
        Some((client, dir, format!("{name}/file").into_bytes(), data))
    }
    fn cleanup(
        client: &mut NfsClient,
        dir: &vfsi_nfs::client::NfsFh,
        path: &[u8],
        opened: Vec<(vfsi_nfs::client::NfsFh, nfsv41_sys::stateid4)>,
    ) {
        let closes: Vec<_> = opened
            .into_iter()
            .map(|(fh, stateid)| CloseOp { fh, stateid })
            .collect();
        client.close_many_path(&closes).unwrap();
        client.remove(dir, "file").unwrap();
        let root = client.root().clone();
        client
            .remove(
                &root,
                std::str::from_utf8(path.split(|&b| b == b'/').next().unwrap()).unwrap(),
            )
            .unwrap();
    }
    #[test]
    fn short_read_keeps_contiguous_data_across_wire_chunks() {
        for close in [false, true] {
            let Some((mut client, dir, path, data)) = fixture("read", close) else {
                return;
            };
            client.test_short_merged_read_once(7);
            let outcome = client
                .readv_path_compound(
                    &[PathReadOp {
                        file: FileRef::Path(path.clone()),
                        offset: 0,
                        count: data.len(),
                        stateid: None,
                    }],
                    close,
                )
                .unwrap();
            assert!(!client.test_merged_short_pending(), "fault must execute");
            let actual = outcome.data[0].clone().unwrap();
            cleanup(&mut client, &dir, &path, outcome.opened);
            assert!(
                actual == data,
                "short READ dropped an interior range: got {}, expected {}",
                actual.len(),
                data.len()
            );
        }
    }
    #[test]
    fn short_write_fills_only_the_missing_wire_suffix() {
        for close in [false, true] {
            let Some((mut client, dir, path, original)) = fixture("write", close) else {
                return;
            };
            let data: Vec<_> = original.iter().map(|b| b ^ 0xff).collect();
            client.test_short_merged_write_once(7);
            let outcome = client
                .writev_path_compound(
                    &[PathWriteOp {
                        file: FileRef::Path(path.clone()),
                        offset: 0,
                        data: &data,
                        create: false,
                        truncate: false,
                        stateid: None,
                    }],
                    close,
                )
                .unwrap();
            assert!(!client.test_merged_short_pending(), "fault must execute");
            // Multiple compounds reuse one path owner. Only the newest
            // stateid for each object remains valid for cleanup.
            let mut seen = std::collections::HashSet::new();
            let closes: Vec<_> = outcome
                .opened
                .iter()
                .rev()
                .filter(|(fh, _)| seen.insert(fh.clone()))
                .map(|(fh, stateid)| CloseOp {
                    fh: fh.clone(),
                    stateid: *stateid,
                })
                .collect();
            client.close_many_path(&closes).unwrap();
            let read = client
                .readv_path_compound(
                    &[PathReadOp {
                        file: FileRef::Path(path.clone()),
                        offset: 0,
                        count: data.len(),
                        stateid: None,
                    }],
                    true,
                )
                .unwrap();
            let actual = read.data[0].clone().unwrap();
            cleanup(&mut client, &dir, &path, Vec::new());
            assert!(
                actual == data,
                "short WRITE left an interior range unchanged"
            );
            assert_eq!(outcome.counts, [Some(data.len() as u32)]);
        }
    }

    #[test]
    fn retained_handle_repairs_keep_the_renamed_object_pinned() {
        for write in [false, true] {
            for close in [false, true] {
                let Some((mut client, dir, path, original)) =
                    fixture(if write { "pinned-write" } else { "pinned-read" }, close)
                else {
                    return;
                };
                let (fh, stateid) = client
                    .open(&dir, b"file", OPEN4_SHARE_ACCESS_BOTH, OpenCreate::NoCreate)
                    .unwrap();
                client.rename(&dir, "file", &dir, "saved").unwrap();
                let (replacement, replacement_state) = client
                    .open(
                        &dir,
                        b"file",
                        OPEN4_SHARE_ACCESS_BOTH,
                        OpenCreate::Unchecked,
                    )
                    .unwrap();
                client
                    .write(&replacement, &replacement_state, 0, b"NEW")
                    .unwrap();
                let expected = if write {
                    original.iter().map(|b| b ^ 0xff).collect::<Vec<_>>()
                } else {
                    original
                };
                if write {
                    client.test_short_merged_write_once(7);
                    let outcome = client
                        .writev_path_compound(
                            &[PathWriteOp {
                                file: FileRef::Handle(fh.clone()),
                                offset: 0,
                                data: &expected,
                                create: false,
                                truncate: false,
                                stateid: Some(stateid),
                            }],
                            close,
                        )
                        .unwrap();
                    assert_eq!(outcome.counts, [Some(expected.len() as u32)]);
                    assert!(outcome.opened.is_empty());
                } else {
                    client.test_short_merged_read_once(7);
                }
                let outcome = client
                    .readv_path_compound(
                        &[PathReadOp {
                            file: FileRef::Handle(fh.clone()),
                            offset: 0,
                            count: expected.len(),
                            stateid: Some(stateid),
                        }],
                        close,
                    )
                    .unwrap();
                assert!(!client.test_merged_short_pending());
                assert!(outcome.opened.is_empty());
                assert_eq!(
                    client
                        .read(&replacement, &replacement_state, 0, 8)
                        .unwrap()
                        .0,
                    b"NEW"
                );
                client.close(&fh, &stateid).unwrap();
                client.close(&replacement, &replacement_state).unwrap();
                client.remove(&dir, "saved").unwrap();
                cleanup(&mut client, &dir, &path, Vec::new());
                assert_eq!(outcome.data[0].as_ref().unwrap(), &expected);
            }
        }
    }
    #[test]
    fn zero_progress_merged_io_is_an_error() {
        for write in [false, true] {
            for close in [false, true] {
                let Some((mut client, dir, path, data)) =
                    fixture(if write { "zero-write" } else { "zero-read" }, close)
                else {
                    return;
                };
                let error = if write {
                    client.test_short_merged_write_once(0);
                    client
                        .writev_path_compound(
                            &[PathWriteOp {
                                file: FileRef::Path(path.clone()),
                                offset: 0,
                                data: &data,
                                create: false,
                                truncate: false,
                                stateid: None,
                            }],
                            close,
                        )
                        .err()
                        .unwrap()
                } else {
                    client.test_short_merged_read_once(0);
                    client
                        .readv_path_compound(
                            &[PathReadOp {
                                file: FileRef::Path(path.clone()),
                                offset: 0,
                                count: data.len(),
                                stateid: None,
                            }],
                            close,
                        )
                        .err()
                        .unwrap()
                };
                assert!(!client.test_merged_short_pending());
                cleanup(&mut client, &dir, &path, Vec::new());
                assert_eq!(error.status, nfsv41_sys::nfsstat4_NFS4ERR_IO);
                assert_eq!(error.op_index, 0);
            }
        }
    }
    #[test]
    fn short_repair_preserves_later_overlapping_write_order() {
        for close in [false, true] {
            let Some((mut client, dir, path, _)) = fixture("overlap", close) else {
                return;
            };
            let first = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            let last = b"ZZZZZZZZ";
            client.test_short_merged_write_once(7);
            let outcome = client
                .writev_path_compound(
                    &[
                        PathWriteOp {
                            file: FileRef::Path(path.clone()),
                            offset: 0,
                            data: first,
                            create: false,
                            truncate: false,
                            stateid: None,
                        },
                        PathWriteOp {
                            file: FileRef::Path(path.clone()),
                            offset: 8,
                            data: last,
                            create: false,
                            truncate: false,
                            stateid: None,
                        },
                    ],
                    close,
                )
                .unwrap();
            let mut seen = std::collections::HashSet::new();
            let closes: Vec<_> = outcome
                .opened
                .iter()
                .rev()
                .filter(|(fh, _)| seen.insert(fh.clone()))
                .map(|(fh, stateid)| CloseOp {
                    fh: fh.clone(),
                    stateid: *stateid,
                })
                .collect();
            client.close_many_path(&closes).unwrap();
            let read = client
                .readv_path_compound(
                    &[PathReadOp {
                        file: FileRef::Path(path.clone()),
                        offset: 0,
                        count: 32,
                        stateid: None,
                    }],
                    true,
                )
                .unwrap();
            cleanup(&mut client, &dir, &path, Vec::new());
            let mut expected = first.to_vec();
            expected[8..16].copy_from_slice(last);
            assert_eq!(read.data[0].as_ref().unwrap(), &expected);
            assert_eq!(outcome.counts, [Some(32), Some(8)]);
        }
    }

    #[test]
    fn short_repair_does_not_overwrite_a_later_failed_alias_prefix() {
        let Some((mut client, dir, path, _)) = fixture("failed-alias", false) else {
            return;
        };
        client.set_max_compound_bytes(2300);
        let fh = client.lookup(&dir, b"file").unwrap();
        client.link(&dir, &fh, "alias").unwrap();
        let parent = &path[..path.len() - 4];
        let mut alias = parent.to_vec();
        alias.extend_from_slice(b"alias");
        let earlier = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let later = vec![b'Z'; client.per_op_bytes() * 2];
        client.test_short_merged_write_once(7);
        client.test_fail_merged_write_once(1, 1);
        let outcome = client.writev_path_compound(
            &[
                PathWriteOp {
                    file: FileRef::Path(path.clone()),
                    offset: 0,
                    data: earlier,
                    create: false,
                    truncate: false,
                    stateid: None,
                },
                PathWriteOp {
                    file: FileRef::Path(alias.clone()),
                    offset: 8,
                    data: &later,
                    create: false,
                    truncate: false,
                    stateid: None,
                },
            ],
            false,
        );
        let read = client
            .readv_path_compound(
                &[PathReadOp {
                    file: FileRef::Path(path.clone()),
                    offset: 0,
                    count: 32,
                    stateid: None,
                }],
                true,
            )
            .unwrap();
        client.remove(&dir, "alias").unwrap();
        cleanup(&mut client, &dir, &path, Vec::new());
        assert_eq!(
            &read.data[0].as_ref().unwrap()[8..32],
            &[b'Z'; 24],
            "earlier gap repair overwrote later accepted alias bytes"
        );
        let error = outcome
            .err()
            .expect("a later attempted alias prevents safe earlier repair");
        assert_eq!(error.status, nfsv41_sys::nfsstat4_NFS4ERR_IO);
        assert_eq!(error.op_index, 0);
    }
    #[test]
    fn independent_merged_ranges_keep_one_compound_without_repairs() {
        let Some((mut client, dir, path, _)) = fixture("batch", true) else {
            return;
        };
        let pieces = [b"first".as_slice(), b"second", b"third"];
        let ops: Vec<_> = pieces
            .iter()
            .enumerate()
            .map(|(index, data)| PathWriteOp {
                file: FileRef::Path(path.clone()),
                offset: index as u64 * 32,
                data,
                create: false,
                truncate: false,
                stateid: None,
            })
            .collect();
        let _ = vfsi_nfs::compound::thread_compound_stats();
        let outcome = client.writev_path_compound(&ops, true).unwrap();
        let writes = vfsi_nfs::compound::thread_compound_stats().0;
        let ops: Vec<_> = pieces
            .iter()
            .enumerate()
            .map(|(index, data)| PathReadOp {
                file: FileRef::Path(path.clone()),
                offset: index as u64 * 32,
                count: data.len(),
                stateid: None,
            })
            .collect();
        let read = client.readv_path_compound(&ops, true).unwrap();
        let reads = vfsi_nfs::compound::thread_compound_stats().0;
        cleanup(&mut client, &dir, &path, outcome.opened);
        assert_eq!(writes, 1);
        assert_eq!(reads, 1);
        assert_eq!(outcome.counts, [Some(5), Some(6), Some(5)]);
        assert_eq!(read.data, pieces.map(|data| Some(data.to_vec())));
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn ordinary_mixed_vectors_keep_one_merged_compound() {
    let Some(server) = server() else { return };
    let mut fs = NfsVecFs::connect_minor(&server, minor()).unwrap();
    let root = format!("/prr-rust11-mixed-batch-{}", std::process::id());
    fs.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
    let first = format!("{root}/first");
    let second = format!("{root}/second");
    let file = fs
        .open_raw_impl(Path::new(&first), libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    let requests = [
        vfsi_sync::WriteOp::new(file.clone(), VfOffset::At(0), b"first".to_vec()),
        vfsi_sync::WriteOp::from_os_path(Path::new(&second), VfOffset::At(0), b"second".to_vec())
            .with_creation(),
    ];
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let result = fs
        .vwrite_impl(&vfsi_sync::test_support::borrow_writes(&requests))
        .unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    let actual = fs.read_raw_impl(&file, 0, 8).unwrap();
    fs.close_impl(&file).unwrap();
    fs.remove_paths_impl(&[Path::new(&root)], true).unwrap();
    assert_eq!(compounds, 1);
    assert_eq!(actual, b"first");
    assert_eq!(
        result.iter().map(|r| r.bytes_written).collect::<Vec<_>>(),
        [5, 6]
    );
}

#[cfg(feature = "test-faults")]
#[test]
fn mixed_append_queries_size_only_once_per_independent_descriptor() {
    let Some(server) = server() else { return };
    let mut counts = Vec::new();
    for append in [false, true] {
        let mut fs = NfsVecFs::connect_minor(&server, minor()).unwrap();
        fs.set_merged_mode("off");
        let root = format!("/prr-rust11-append-count-{}-{append}", std::process::id());
        fs.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
        let first = format!("{root}/first");
        let second = format!("{root}/second");
        let file = fs
            .open_raw_impl(
                Path::new(&first),
                libc::O_CREAT | libc::O_RDWR | if append { libc::O_APPEND } else { 0 },
                0o600,
            )
            .unwrap();
        let requests = [
            vfsi_sync::WriteOp::new(file.clone(), VfOffset::At(0), b"x".to_vec()),
            vfsi_sync::WriteOp::from_os_path(Path::new(&second), VfOffset::At(0), b"y".to_vec())
                .with_creation(),
        ];
        let _ = vfsi_nfs::compound::thread_compound_stats();
        fs.vwrite_impl(&vfsi_sync::test_support::borrow_writes(&requests))
            .unwrap();
        counts.push(vfsi_nfs::compound::thread_compound_stats().0);
        fs.close_impl(&file).unwrap();
        fs.remove_paths_impl(&[Path::new(&root)], true).unwrap();
    }
    assert_eq!(
        counts[1],
        counts[0] + 1,
        "append must add exactly one GETATTR, counts={counts:?}"
    );
}

#[test]
fn mixed_append_respects_later_path_truncation_boundaries() {
    let Some(server) = server() else { return };
    for mode in ["full", "openwrite", "off"] {
        for alias in [false, true] {
            for reverse in [false, true] {
                let mut fs = NfsVecFs::connect_minor(&server, minor()).unwrap();
                fs.set_merged_mode(mode);
                let root = format!(
                    "/prr-rust11-trunc-{}-{mode}-{alias}-{reverse}",
                    std::process::id()
                );
                fs.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
                let first = format!("{root}/first");
                let link = format!("{root}/alias");
                let file = fs
                    .open_raw_impl(
                        Path::new(&first),
                        libc::O_CREAT | libc::O_RDWR | libc::O_APPEND,
                        0o600,
                    )
                    .unwrap();
                fs.write_raw_impl(&file, 0, b"original").unwrap();
                if alias {
                    fs.vhardlink_impl(&[Path::new(&first)], &[Path::new(&link)])
                        .unwrap();
                }
                let mut requests = [
                    vfsi_sync::WriteOp::new(file.clone(), VfOffset::At(0), b"long-append".to_vec()),
                    vfsi_sync::WriteOp::from_os_path(
                        Path::new(if alias { &link } else { &first }),
                        VfOffset::At(0),
                        b"Z".to_vec(),
                    )
                    .with_truncate(),
                ];
                if reverse {
                    requests.reverse();
                }
                fs.vwrite_impl(&vfsi_sync::test_support::borrow_writes(&requests))
                    .unwrap();
                let actual = fs.read_raw_impl(&file, 0, 32).unwrap();
                fs.close_impl(&file).unwrap();
                fs.remove_paths_impl(&[Path::new(&root)], true).unwrap();
                assert_eq!(
                    actual,
                    if reverse {
                        b"Zlong-append".as_slice()
                    } else {
                        b"Z"
                    },
                    "mode={mode}, alias={alias}, reverse={reverse}"
                );
            }
        }
    }
}
