//! Opt-in development measurement; this does not change production READDIR masks.
use super::*;
use crate::client::DirEntry;
use crate::telemetry::Telemetry;
use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

const PAGE_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy)]
struct Strategy {
    handles: bool,
    width: usize,
    stat: bool,
    page_bytes: usize,
}
impl Strategy {
    fn label(self) -> &'static str {
        if self.handles {
            "returned_fh"
        } else {
            "parent_name"
        }
    }
    fn attrs(self) -> &'static [u32] {
        match (self.stat, self.handles) {
            (false, false) => &[1],
            (false, true) => &[1, 19],
            (true, false) => &[1, 4, 20, 33, 35, 36, 37, 45, 47, 52, 53],
            (true, true) => &[1, 4, 19, 20, 33, 35, 36, 37, 45, 47, 52, 53],
        }
    }
    fn profile(self) -> &'static str {
        if self.stat { "c_stat" } else { "type_only" }
    }
}

struct Work {
    parent: NfsFh,
    name: Vec<u8>,
    relative: Vec<u8>,
    handle: Option<NfsFh>,
}

#[derive(Default)]
struct Scan {
    entries: BTreeMap<Vec<u8>, u32>,
    attr_bytes: usize,
    pending: VecDeque<Work>,
}

/// FILEHANDLE follows TYPE, plus SIZE in the C-stat mask. Missing handles
/// fail the experiment rather than allowing fallback to contaminate a sample.
fn entry_fields(entry: &DirEntry, strategy: Strategy) -> (u32, Option<NfsFh>) {
    let attrs = &entry.attrs;
    let kind = u32::from_be_bytes(attrs[..4].try_into().unwrap());
    if !strategy.handles {
        if !strategy.stat {
            assert_eq!(attrs.len(), 4, "unexpected TYPE attribute encoding");
        }
        return (kind, None);
    }
    let offset = if strategy.stat { 12 } else { 4 };
    let length = u32::from_be_bytes(attrs[offset..offset + 4].try_into().unwrap()) as usize;
    assert!((1..=128).contains(&length), "invalid returned filehandle");
    let end = offset + 4 + length.div_ceil(4) * 4;
    if strategy.stat {
        assert!(attrs.len() > end, "missing attributes after FILEHANDLE");
    } else {
        assert_eq!(attrs.len(), end);
    }
    // Regular-file handles arrive on the wire but are never retained by traversal.
    let handle = (kind == nfs_ftype4_NF4DIR)
        .then(|| NfsFh::test_from_bytes(attrs[offset + 4..offset + 4 + length].to_vec()));
    (kind, handle)
}

impl Scan {
    fn directory(
        &mut self,
        backend: &mut NfsVecFs,
        handle: &NfsFh,
        relative: &[u8],
        mut first: (Vec<DirEntry>, u64),
        strategy: Strategy,
    ) {
        loop {
            let (entries, cookie) = first;
            for entry in entries {
                let (kind, child_handle) = entry_fields(&entry, strategy);
                let mut path = relative.to_vec();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(&entry.name);
                self.attr_bytes += entry.attrs.len();
                assert!(
                    self.entries.insert(path.clone(), kind).is_none(),
                    "duplicate entry"
                );
                if kind == nfs_ftype4_NF4DIR {
                    self.pending.push_back(Work {
                        parent: handle.clone(),
                        name: entry.name,
                        relative: path,
                        handle: child_handle,
                    });
                }
            }
            if cookie == 0 {
                return;
            }
            first = {
                let page_ops = &[(handle.clone(), cookie)];
                crate::client::collect_rpc_results(
                    backend.nfs.readdir_pages_bounded(
                        page_ops,
                        strategy.attrs(),
                        strategy.page_bytes,
                    ),
                    page_ops.len(),
                )
            }
            .unwrap()
            .pop()
            .unwrap();
        }
    }
}

fn scan(backend: &mut NfsVecFs, root: &NfsFh, strategy: Strategy) -> Scan {
    let mut result = Scan::default();
    let first = {
        let page_ops = &[(root.clone(), 0)];
        crate::client::collect_rpc_results(
            backend
                .nfs
                .readdir_pages_bounded(page_ops, strategy.attrs(), strategy.page_bytes),
            page_ops.len(),
        )
    }
    .unwrap()
    .pop()
    .unwrap();
    result.directory(backend, root, b"", first, strategy);
    while !result.pending.is_empty() {
        let batch: Vec<_> = result
            .pending
            .drain(..result.pending.len().min(strategy.width))
            .collect();
        if strategy.handles {
            let ops: Vec<_> = batch
                .iter()
                .map(|work| (work.handle.clone().unwrap(), 0))
                .collect();
            let pages = {
                let page_ops = &ops;
                crate::client::collect_rpc_results(
                    backend.nfs.readdir_pages_bounded(
                        page_ops,
                        strategy.attrs(),
                        strategy.page_bytes,
                    ),
                    page_ops.len(),
                )
            }
            .unwrap();
            assert_eq!(pages.len(), batch.len());
            for ((work, (handle, _)), page) in batch.into_iter().zip(ops).zip(pages) {
                result.directory(backend, &handle, &work.relative, page, strategy);
            }
        } else {
            let ops: Vec<_> = batch
                .iter()
                .map(|work| (work.parent.clone(), work.name.clone()))
                .collect();
            let pages = {
                let page_ops = &ops;
                crate::client::collect_rpc_results(
                    backend.nfs.readdir_children_bounded(
                        page_ops,
                        strategy.attrs(),
                        strategy.page_bytes,
                    ),
                    page_ops.len(),
                )
            }
            .unwrap();
            assert_eq!(pages.len(), batch.len());
            for (work, page) in batch.into_iter().zip(pages) {
                result.directory(
                    backend,
                    &page.fh,
                    &work.relative,
                    (page.entries, page.cookie),
                    strategy,
                );
            }
        }
    }
    result
}

struct Fixture(PathBuf);
impl Fixture {
    fn create(mount: &Path, directories: usize, files: usize) -> (Self, String) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("vfsi-directory-measurement-{}-{unique}", std::process::id());
        let path = mount.join(&name);
        std::fs::create_dir(&path).unwrap();
        let fixture = Self(path);
        for index in 0..directories {
            let child = fixture.0.join(format!("d-{index:04}"));
            std::fs::create_dir(&child).unwrap();
            for leaf in 0..8 {
                std::fs::write(child.join(format!("leaf-{leaf:02}")), b"x").unwrap();
            }
        }
        for index in 0..files {
            std::fs::write(fixture.0.join(format!("file-{index:04}")), b"x").unwrap();
        }
        (fixture, name)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove only the benchmark's owned fixture");
    }
}

#[test]
#[ignore = "manual writable-NFS measurement; requires VFSI_NFS_SERVER and VFSI_NFS_TEST_MOUNT"]
fn compare_returned_filehandles_to_parent_name_seeds() {
    let host = std::env::var("VFSI_NFS_SERVER").expect("set VFSI_NFS_SERVER");
    let mount =
        PathBuf::from(std::env::var_os("VFSI_NFS_TEST_MOUNT").expect("set mount of export root"));
    let minor = std::env::var("VFSI_NFS_MINOR")
        .expect("set VFSI_NFS_MINOR")
        .parse::<u32>()
        .unwrap();
    assert!(matches!(minor, 1 | 2));
    let telemetry = Telemetry::default();
    let mut backend = NfsClientBuilder::new(host)
        .minor_version(Some(minor))
        .telemetry(telemetry.clone())
        .connect()
        .unwrap();
    let mut strategies = [
        Strategy {
            handles: false,
            width: 24,
            stat: false,
            page_bytes: PAGE_BYTES,
        },
        Strategy {
            handles: true,
            width: 24,
            stat: false,
            page_bytes: PAGE_BYTES,
        },
        Strategy {
            handles: true,
            width: 32,
            stat: false,
            page_bytes: PAGE_BYTES,
        },
    ];
    for (case, directories, files) in [("directory_dense", 64, 0), ("file_dense", 4, 4096)] {
        let (_fixture, name) = Fixture::create(&mount, directories, files);
        // Resolve once outside timing: both variants start with the same root FH.
        let root = backend.nfs.resolve(name.as_bytes()).unwrap();
        let expected = scan(&mut backend, &root, strategies[0]).entries;
        assert_eq!(expected.len(), directories * 9 + files);
        let c_fields = AttrMask::MODE
            | AttrMask::SIZE
            | AttrMask::NLINK
            | AttrMask::FILEID
            | AttrMask::BLOCKS
            | AttrMask::UID
            | AttrMask::GID
            | AttrMask::ATIME
            | AttrMask::MTIME
            | AttrMask::CTIME;
        for stat in [false, true] {
            for strategy in &mut strategies {
                strategy.stat = stat;
            }
            if stat {
                assert_eq!(strategies[0].attrs(), request_mask_to_attr_list(&c_fields));
            }
            // Warm all tagged compound planners and server data before paired samples.
            for strategy in strategies {
                assert_eq!(scan(&mut backend, &root, strategy).entries, expected);
            }
            for sample in 0..6 {
                // Alternate forward/reverse order to avoid always measuring a candidate last.
                let order = if sample % 2 == 0 {
                    [0, 1, 2]
                } else {
                    [2, 1, 0]
                };
                for index in order {
                    let strategy = strategies[index];
                    let before = telemetry.snapshot();
                    let start = Instant::now();
                    let result = scan(&mut backend, &root, strategy);
                    let elapsed = start.elapsed().as_micros();
                    let after = telemetry.snapshot();
                    assert_eq!(after.transport_errors, before.transport_errors);
                    assert_eq!(after.nfs_errors, before.nfs_errors);
                    assert_eq!(result.entries, expected, "complete traversal parity");
                    println!(
                        "READDIR_HANDLES {{\"minor\":{minor},\"fixture\":\"{case}\",\"profile\":\"{}\",\"strategy\":\"{}\",\"width\":{},\"sample\":{sample},\"rpc_attempts\":{},\"operations\":{},\"request_bytes\":{},\"returned_attr_bytes\":{},\"entries\":{},\"elapsed_us\":{elapsed}}}",
                        strategy.profile(),
                        strategy.label(),
                        strategy.width,
                        after.calls - before.calls,
                        after.operations - before.operations,
                        after.request_bytes - before.request_bytes,
                        result.attr_bytes,
                        result.entries.len(),
                    );
                }
            }
        }
    }
}

#[cfg(feature = "test-faults")]
#[test]
#[ignore = "manual writable-NFS measurement; requires server and matching export-root mount"]
fn compare_parent_name_cohorts_and_page_budgets() {
    let host = std::env::var("VFSI_NFS_SERVER").expect("set VFSI_NFS_SERVER");
    let mount =
        PathBuf::from(std::env::var_os("VFSI_NFS_TEST_MOUNT").expect("set export-root mount"));
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert!(matches!(minor, 1 | 2));
    for (case, directories, files) in [("directory_dense", 128, 0), ("file_dense", 4, 4096)] {
        let (_fixture, name) = Fixture::create(&mount, directories, files);
        let mut strategies: Vec<_> = [8, 16, 24, 31, 32]
            .into_iter()
            .flat_map(|width| {
                [4096, 16384, 32768, 65536]
                    .into_iter()
                    .map(move |page_bytes| Strategy {
                        handles: false,
                        width,
                        stat: true,
                        page_bytes,
                    })
            })
            .collect();
        // Avoid always visiting the same shape last across protocol/fixture runs.
        if (minor == 2) != (files != 0) {
            strategies.reverse();
        }
        for operation_limit in [0, 33] {
            for &strategy in &strategies {
                // Adaptive budgets are tagged per client. Isolate each setting
                // so one rejected shape cannot shrink another setting's planner.
                let telemetry = Telemetry::default();
                let mut backend = NfsClientBuilder::new(host.clone())
                    .minor_version(Some(minor))
                    .telemetry(telemetry.clone())
                    .connect()
                    .unwrap();
                if operation_limit != 0 {
                    backend.nfs.test_limit_compound_operations(operation_limit);
                }
                let root = backend.nfs.resolve(name.as_bytes()).unwrap();
                let expected = scan(&mut backend, &root, strategy).entries;
                assert_eq!(expected.len(), directories * 9 + files);
                for _ in 0..2 {
                    assert_eq!(scan(&mut backend, &root, strategy).entries, expected);
                }
                for sample in 0..3 {
                    let before = telemetry.snapshot();
                    let start = Instant::now();
                    let result = scan(&mut backend, &root, strategy);
                    let elapsed = start.elapsed().as_micros();
                    let after = telemetry.snapshot();
                    assert_eq!(
                        result.entries, expected,
                        "complete name/type traversal parity"
                    );
                    assert_eq!(after.transport_errors, before.transport_errors);
                    // Retain any safe RESOURCE/TOO_MANY_OPS repacking attempts
                    // in the measured result; a successful scan is mandatory.
                    println!(
                        "READDIR_COHORTS {{\"minor\":{minor},\"fixture\":\"{case}\",\"operation_limit\":{operation_limit},\"width\":{},\"page_bytes\":{},\"sample\":{sample},\"rpc_attempts\":{},\"nfs_rejections\":{},\"operations\":{},\"request_bytes\":{},\"returned_attr_bytes\":{},\"entries\":{},\"elapsed_us\":{elapsed}}}",
                        strategy.width,
                        strategy.page_bytes,
                        after.calls - before.calls,
                        after.nfs_errors - before.nfs_errors,
                        after.operations - before.operations,
                        after.request_bytes - before.request_bytes,
                        result.attr_bytes,
                        result.entries.len(),
                    );
                }
            }
        }
    }
}
