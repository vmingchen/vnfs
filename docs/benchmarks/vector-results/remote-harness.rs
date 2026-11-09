use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;
use vnfs::files::{OpenFlags, OpenOp, ReadOp, Vfsi, VfsiExt};
static CALLS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
struct Count;
unsafe impl GlobalAlloc for Count {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            CALLS.fetch_add(1, Relaxed);
            BYTES.fetch_add(l.size(), Relaxed);
            let n = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(n, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() {
            CALLS.fetch_add(1, Relaxed);
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
static ALLOC: Count = Count;
fn run(
    fs: &impl Vfsi,
    root: &std::path::Path,
    mixed: bool,
    get_calls: impl Fn() -> u64,
) -> serde_json::Value {
    let paths: Vec<_> = (0..64).map(|i| format!("/file-{i}")).collect();
    let requests = paths
        .iter()
        .map(|p| OpenOp::new(p, OpenFlags::READ))
        .collect::<Vec<_>>();
    #[cfg(feature = "before")]
    let mut files = fs.vopen(&requests).unwrap();
    #[cfg(not(feature = "before"))]
    let mut files = fs.open_batch(&requests).unwrap();
    if mixed {
        #[cfg(feature = "before")]
        fs.vclose(&mut files[32..33]).unwrap();
        #[cfg(not(feature = "before"))]
        fs.close_batch(&mut files[32..33]).unwrap();
    }
    for _ in 0..10 {
        let _ = std::hint::black_box(fs.vread(
            files.iter().map(|f| ReadOp::range(f, 0, 4096)),
            Default::default(),
        ));
    }
    let start_live = LIVE.load(Relaxed);
    PEAK.store(start_live, Relaxed);
    let calls = CALLS.load(Relaxed);
    let allocated = BYTES.load(Relaxed);
    let mut usage0 = unsafe { std::mem::zeroed::<libc::rusage>() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage0) };
    let rpc_start = get_calls();
    let start = Instant::now();
    let mut successful = 0usize;
    let mut failed = 0usize;
    let mut returned_bytes = 0usize;
    for _ in 0..200 {
        let result = fs.vread(
            files.iter().map(|f| ReadOp::range(f, 0, 4096)),
            Default::default(),
        );
        if let Ok(results) = &result {
            #[cfg(feature = "before")]
            {
                successful += results.len();
                for r in results {
                    returned_bytes += r.bytes_read();
                }
            }
            #[cfg(not(feature = "before"))]
            for r in results {
                match r {
                    Ok(r) => {
                        successful += 1;
                        returned_bytes += r.bytes_read();
                    }
                    Err(_) => failed += 1,
                }
            }
        } else {
            failed += 1;
        }
        std::hint::black_box(&result);
    }
    let wall = start.elapsed().as_nanos();
    let rpc_calls = get_calls() - rpc_start;
    let allocations = CALLS.load(Relaxed) - calls;
    let allocated_bytes = BYTES.load(Relaxed) - allocated;
    let peak_live = PEAK.load(Relaxed).saturating_sub(start_live);
    let mut usage1 = unsafe { std::mem::zeroed::<libc::rusage>() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage1) };
    let micros = |t: libc::timeval| t.tv_sec as i64 * 1_000_000 + t.tv_usec as i64;
    let cpu = micros(usage1.ru_utime) + micros(usage1.ru_stime)
        - micros(usage0.ru_utime)
        - micros(usage0.ru_stime);
    fs.close_files(files).unwrap();
    let _ = root;
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let peak_rss = status
        .lines()
        .find(|line| line.starts_with("VmHWM:"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<usize>()
        .unwrap();
    serde_json::json!({"wall_ns":wall,"rpc_calls":rpc_calls,"cpu_us":cpu,"allocations":allocations,"allocated_bytes":allocated_bytes,"peak_live_delta":peak_live,"peak_rss_kib":peak_rss,"successful":successful,"failed":failed,"returned_bytes":returned_bytes})
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let root = std::path::Path::new(&args[1]);
    let mixed = args[3] == "mixed";
    let out = if args[2] == "smb" {
        let backend = vfsi_smb::SmbVecFs::connect(
            &std::env::var("VFSI_SMB_SERVER").unwrap(),
            &std::env::var("VFSI_SMB_SHARE").unwrap(),
            &std::env::var("VFSI_SMB_USERNAME").unwrap_or_default(),
            &std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default(),
            &std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default(),
        )
        .unwrap();
        let fs = vfsi_sync::FsClient::new(backend);
        let mut out = run(&fs, root, mixed, || 0);
        out["rpc_calls"] = serde_json::Value::Null;
        out
    } else {
        let t = vnfs::nfs::Telemetry::default();
        let version = if args[2] == "nfs41" {
            vnfs::nfs::NfsVersion::V4_1
        } else {
            vnfs::nfs::NfsVersion::V4_2
        };
        let fs = vnfs::nfs::Nfs::builder(
            std::env::var("VFSI_NFS_SERVER").unwrap_or_else(|_| "127.0.0.1:2049".into()),
        )
        .root(root)
        .version(version)
        .telemetry(t.clone())
        .connect()
        .unwrap();
        run(&fs, root, mixed, || t.snapshot().calls)
    };
    println!("{out}");
}
