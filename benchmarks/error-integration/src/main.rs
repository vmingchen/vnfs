//! Integrated measurements of the actual vector clients. No substitute error type.
use serde_json::json;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
    time::Instant,
};
use vfsi_core::{
    FailureState, OpError, StatusCode,
    api::{OpenFlags, OpenOp, ReadOp, ReadOptions, SyncMode, Vfsi, VfsiExt},
};
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "allocation-profile")]
static PROFILE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(feature = "allocation-profile")]
static SIZES: [AtomicUsize; 65537] = [const { AtomicUsize::new(0) }; 65537];
#[cfg(feature = "allocation-stats")]
fn record_size(_size: usize) {
    #[cfg(feature = "allocation-profile")]
    if PROFILE.load(Relaxed) {
        SIZES[_size.min(65536)].fetch_add(1, Relaxed);
    }
}
struct Count;
// SAFETY: allocation contracts pass unchanged to System; counters use allocation-free atomics.
unsafe impl GlobalAlloc for Count {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: forward the caller-provided allocation layout unchanged.
        let p = unsafe { System.alloc(l) };
        #[cfg(feature = "allocation-stats")]
        if !p.is_null() {
            record_size(l.size());
            ALLOCS.fetch_add(1, Relaxed);
            BYTES.fetch_add(l.size(), Relaxed);
            let n = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(n, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        #[cfg(feature = "allocation-stats")]
        LIVE.fetch_sub(l.size(), Relaxed);
        // SAFETY: forward the caller-provided live pointer and matching layout.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        // SAFETY: forward the live pointer, old layout and requested size unchanged.
        let q = unsafe { System.realloc(p, l, n) };
        #[cfg(feature = "allocation-stats")]
        if !q.is_null() {
            record_size(n);
            ALLOCS.fetch_add(1, Relaxed);
            BYTES.fetch_add(n, Relaxed);
            let live = if n >= l.size() {
                LIVE.fetch_add(n - l.size(), Relaxed) + n - l.size()
            } else {
                LIVE.fetch_sub(l.size() - n, Relaxed) - (l.size() - n)
            };
            PEAK.fetch_max(live, Relaxed);
        }
        q
    }
}
#[global_allocator]
static ALLOCATOR: Count = Count;
#[cfg(feature = "standard-error")]
type Cause = std::io::Error;
#[cfg(not(feature = "standard-error"))]
type Cause = vfsi_core::VfError;
#[cfg(feature = "standard-error")]
fn native(e: &Cause) -> Option<StatusCode> {
    vfsi_core::native_status(e)
}
#[cfg(not(feature = "standard-error"))]
fn native(e: &Cause) -> Option<StatusCode> {
    e.status()
}
#[cfg(feature = "standard-error")]
fn fanout(e: Cause) -> Vec<Cause> {
    vfsi_core::internal::errors::fanout(e, 64).collect()
}
#[cfg(not(feature = "standard-error"))]
fn fanout(e: Cause) -> Vec<Cause> {
    (0..64).map(|_| e.clone()).collect()
}
#[cfg(feature = "standard-error")]
fn nfs_cause() -> Cause {
    vfsi_core::internal::errors::from_status(StatusCode::Nfs(13))
}
#[cfg(not(feature = "standard-error"))]
fn nfs_cause() -> Cause {
    Cause::nfs(None, 13)
}
#[cfg(feature = "standard-error")]
fn transport(e: &Cause) -> Option<vfsi_core::TransportKind> {
    vfsi_core::transport_kind(e)
}
#[cfg(not(feature = "standard-error"))]
fn transport(e: &Cause) -> Option<vfsi_core::TransportKind> {
    e.transport_kind()
}
#[derive(Debug)]
struct Payload {
    bytes: Vec<u8>,
    drops: Arc<AtomicUsize>,
}
impl std::fmt::Display for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "opaque {}", self.bytes.len())
    }
}
impl std::error::Error for Payload {}
impl Drop for Payload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Relaxed);
    }
}
fn opaque_cause(drops: &Arc<AtomicUsize>) -> Cause {
    let e = std::io::Error::other(Payload {
        bytes: vec![9; 256],
        drops: drops.clone(),
    });
    #[cfg(feature = "standard-error")]
    {
        e
    }
    #[cfg(not(feature = "standard-error"))]
    {
        Cause::from_io(None, e)
    }
}
fn measure(mut work: impl FnMut() -> u64, count: usize) -> serde_json::Value {
    let base = LIVE.load(Relaxed);
    ALLOCS.store(0, Relaxed);
    BYTES.store(0, Relaxed);
    PEAK.store(base, Relaxed);
    #[cfg(feature = "allocation-profile")]
    {
        for size in &SIZES {
            size.store(0, Relaxed);
        }
        PROFILE.store(true, Relaxed);
    }
    let start = Instant::now();
    let mut digest = 0;
    for _ in 0..count {
        digest ^= black_box(work());
    }
    let elapsed = start.elapsed().as_nanos();
    #[cfg(feature = "allocation-profile")]
    PROFILE.store(false, Relaxed);
    let a = ALLOCS.load(Relaxed);
    let b = BYTES.load(Relaxed);
    let peak = PEAK.load(Relaxed);
    let after = LIVE.load(Relaxed);
    let row = json!({"iterations":count,"ns":elapsed,"ns_per_iteration":elapsed as f64/count as f64,"allocation_instrumented":cfg!(feature="allocation-stats"),"allocation_calls":a,"allocated_bytes":b,"peak_extra_live_bytes":peak.saturating_sub(base),"retained_delta":after as i128-base as i128,"digest":digest});
    #[cfg(feature = "allocation-profile")]
    {
        let mut row = row;
        row["allocation_sizes"] = json!(
            SIZES
                .iter()
                .enumerate()
                .filter_map(|(size, n)| {
                    let n = n.load(Relaxed);
                    (n > 0).then_some((size, n))
                })
                .collect::<Vec<_>>()
        );
        row
    }
    #[cfg(not(feature = "allocation-profile"))]
    row
}
fn representation(scenario: &str, count: usize) -> serde_json::Value {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut row = measure(
        || {
            let causes = fanout(if scenario == "fanout_nfs" {
                nfs_cause()
            } else {
                opaque_cause(&drops)
            });
            assert_eq!(causes.len(), 64);
            let outcomes: Vec<Result<(), OpError>> = causes
                .into_iter()
                .map(|e| Err(OpError::failed(e, None)))
                .collect();
            for e in &outcomes {
                let e = e.as_ref().unwrap_err();
                assert_eq!(e.state(), FailureState::Failed);
                if scenario == "fanout_nfs" {
                    assert_eq!(native(e.cause()), Some(StatusCode::Nfs(13)));
                }
            }
            drop(outcomes);
            64
        },
        count,
    );
    if scenario == "fanout_opaque" {
        assert_eq!(drops.load(Relaxed), count);
    }
    row["released_sources"] = json!(drops.load(Relaxed));
    row
}
type FailureFact = (FailureState, std::io::ErrorKind, Option<StatusCode>);

fn run(
    fs: impl Vfsi,
    root: &Path,
    scenario: &str,
    count: usize,
    ring: Option<vfsi_uring::Telemetry>,
) -> serde_json::Value {
    let paths: Vec<_> = (0..64).map(|i| root.join(format!("file-{i}"))).collect();
    if scenario == "prepare" {
        fs.create_dir(root).unwrap();
        let payloads: Vec<_> = (0..64).map(|i| vec![i as u8; 4096]).collect();
        let entries: Vec<_> = paths.iter().zip(&payloads).collect();
        fs.write_files(&entries).unwrap();
        return json!({"prepared":64});
    }
    let failed = if scenario == "all" {
        64
    } else if matches!(scenario, "mixed" | "lossy") {
        1
    } else {
        0
    };
    let bad = |i: usize| failed == 64 || (failed == 1 && i == 32);
    let missing: Vec<_> = (0..64).map(|i| root.join(format!("missing-{i}"))).collect();
    let by_path = root != Path::new("/");
    let requests: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| {
            OpenOp::new(
                p,
                if scenario == "void" || (bad(i) && !by_path) {
                    OpenFlags::WRITE
                } else {
                    OpenFlags::READ
                },
            )
        })
        .collect();
    let mut handles: Vec<_> = fs
        .vopen(&requests)
        .unwrap()
        .into_iter()
        .map(Result::unwrap)
        .collect();
    let refs: Vec<_> = handles.iter().collect();
    let mut facts: Option<[Option<FailureFact>; 64]> = None;
    let mut selected_error = None;
    let mut work = || {
        if scenario == "void" {
            let r = fs.vfsync(&refs, SyncMode::All).unwrap();
            assert_eq!(r.len(), 64);
            assert!(r.into_iter().all(|r| r.is_ok()));
            return 64;
        }
        let ops = handles.iter().enumerate().map(|(i, h)| {
            if by_path && bad(i) {
                ReadOp::whole(&missing[i])
            } else {
                ReadOp::range(h, 0, 4096)
            }
        });
        if scenario == "lossy" {
            let error = fs.read_batch(ops, ReadOptions::new()).unwrap_err();
            #[cfg(feature = "standard-error")]
            let cause = error.cause();
            #[cfg(not(feature = "standard-error"))]
            let cause = &error;
            assert_eq!(error.index(), Some(32));
            let status = native(cause);
            if by_path {
                assert_eq!(cause.kind(), std::io::ErrorKind::NotFound);
                assert!(matches!(
                    status,
                    Some(StatusCode::Nfs(2) | StatusCode::Smb(0xc000_0034))
                ));
            } else {
                assert_eq!(status, Some(StatusCode::Errno(9)));
                assert_eq!(cause.kind(), std::io::Error::from_raw_os_error(9).kind());
            }
            assert_eq!(transport(cause), None);
            selected_error = Some((error.index(), cause.kind(), status, transport(cause)));

            return 63 * 4096;
        }
        let results = fs.vread(ops, ReadOptions::new()).unwrap();
        assert_eq!(results.len(), 64);
        let mut errors = [None; 64];
        let mut bytes = 0;
        for (i, result) in results.into_iter().enumerate() {
            match result {
                Ok(r) => {
                    assert!(!bad(i), "unexpected success at {i}");
                    assert_eq!(r.offset(), 0);
                    assert_eq!(r.bytes_read(), 4096);
                    let data = r.data().unwrap();
                    assert_eq!(data[0], i as u8);
                    assert_eq!(data[4095], i as u8);
                    bytes += r.bytes_read();
                }
                Err(e) => {
                    assert!(bad(i), "unexpected failure {i}: {e:?}");
                    assert!(matches!(
                        e.state(),
                        FailureState::Failed | FailureState::NotAttempted
                    ));
                    if let Some(vfsi_core::IoProgress::Read(p)) = e.progress() {
                        assert_eq!(p.bytes_read(), 0);
                    }
                    errors[i] = Some((e.state(), e.cause().kind(), native(e.cause())));
                }
            }
        }
        assert_eq!(bytes, (64 - failed) * 4096);
        if let Some(previous) = &facts {
            assert_eq!(previous, &errors)
        } else {
            facts = Some(errors);
        }
        bytes as u64
    };
    // Warm actual clients before counter snapshots; allocation counting and timing use separate binaries.
    for _ in 0..20 {
        black_box(work());
    }
    let before = ring.as_ref().map(|t| t.snapshot());
    vfsi_nfs::compound::compound_stats();
    vfsi_nfs::compound::rpc_stats();
    let mut row = measure(&mut work, count);
    drop(work);
    let after = ring.as_ref().map(|t| t.snapshot());
    let (compounds, operations, request_bytes, max_ops) = vfsi_nfs::compound::compound_stats();
    let (rpcs, _) = vfsi_nfs::compound::rpc_stats();
    row["nfs"] = json!({"rpcs":rpcs,"compounds":compounds,"operations":operations,"request_bytes":request_bytes,"max_ops":max_ops});
    if let (Some(a), Some(b)) = (before, after) {
        row["ring"] = json!({"sqes":b.submissions-a.submissions,"cqes":b.completions-a.completions,"waves":b.waves-a.waves,"enters":b.enters-a.enters,"retained_descriptors":b.retained_descriptors,"retained_buffer_bytes":b.retained_buffer_bytes});
    }
    row["selected_error"] = json!(format!("{:?}", selected_error));
    row["facts"] = json!(format!("{:?}", facts));
    row["successes_per_iteration"] = json!(64 - failed);
    row["failures_per_iteration"] = json!(failed);
    drop(refs);
    let outcomes = fs.vclose(&mut handles).unwrap();
    assert!(outcomes.into_iter().all(|r| r.is_ok()));
    row["closed_handles"] = json!(64);
    row
}
fn main() {
    let a: Vec<_> = std::env::args().collect();
    let backend = &a[1];
    let scenario = &a[2];
    let count = a[3].parse().unwrap();
    let root = Path::new(&a[4]);
    let mut row = if scenario.starts_with("fanout_") {
        representation(scenario, count)
    } else {
        match backend.as_str() {
            "posix" => run(
                vfsi_posix::connect(root).unwrap(),
                Path::new("/"),
                scenario,
                count,
                None,
            ),
            "uring" => {
                let (fs, t) = vfsi_uring::connect_with_telemetry(root, Default::default()).unwrap();
                run(fs, Path::new("/"), scenario, count, Some(t))
            }
            "nfs41" | "nfs42" => {
                let version = if backend == "nfs41" {
                    vnfs::nfs::NfsVersion::V4_1
                } else {
                    vnfs::nfs::NfsVersion::V4_2
                };
                run(
                    vnfs::nfs::Nfs::builder("127.0.0.1:2049")
                        .version(version)
                        .connect()
                        .unwrap(),
                    root,
                    scenario,
                    count,
                    None,
                )
            }
            "smb" => {
                let fs = vfsi_sync::FsClient::new(
                    vfsi_smb::SmbVecFs::connect("127.0.0.1", "vfsi-test", "", "", "").unwrap(),
                );
                run(fs, root, scenario, count, None)
            }
            _ => panic!("unknown backend"),
        }
    };
    row["backend"] = json!(backend);
    row["scenario"] = json!(scenario);
    row["cause_bytes"] = json!(std::mem::size_of::<Cause>());
    row["op_error_bytes"] = json!(std::mem::size_of::<OpError>());
    println!("{row}");
}
