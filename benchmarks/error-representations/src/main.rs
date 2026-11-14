//! Isolated representation experiment, not a replacement vnfs backend.
//! The standard candidate uses compact typed payloads, not strings or errno
//! reinterpretation for protocol statuses. Timing builds omit allocation counts.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    borrow::Cow,
    error::Error,
    fmt,
    fs::File,
    hint::black_box,
    io::{self, ErrorKind as K},
    mem::size_of,
    os::{fd::AsRawFd, unix::fs::FileExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    },
    time::Instant,
};
use vfsi_core::{Attrs, FailureState, IoProgress, StatusCode, TransportKind, api::ReadResult};

static TRACK: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
struct Count;
unsafe impl GlobalAlloc for Count {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: delegate the allocator contract unchanged.
        let ptr = unsafe { System.alloc(layout) };
        #[cfg(feature = "allocation-stats")]
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Relaxed) + layout.size();
            PEAK.fetch_max(live, Relaxed);
            if TRACK.load(Relaxed) {
                ALLOCS.fetch_add(1, Relaxed);
                BYTES.fetch_add(layout.size(), Relaxed);
            }
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        #[cfg(feature = "allocation-stats")]
        LIVE.fetch_sub(layout.size(), Relaxed);
        // SAFETY: delegate the allocator contract unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, size: usize) -> *mut u8 {
        // SAFETY: delegate the allocator contract unchanged.
        let result = unsafe { System.realloc(ptr, old, size) };
        #[cfg(feature = "allocation-stats")]
        if !result.is_null() {
            let live = if size >= old.size() {
                LIVE.fetch_add(size - old.size(), Relaxed) + size - old.size()
            } else {
                LIVE.fetch_sub(old.size() - size, Relaxed) - (old.size() - size)
            };
            PEAK.fetch_max(live, Relaxed);
            if TRACK.load(Relaxed) {
                ALLOCS.fetch_add(1, Relaxed);
                BYTES.fetch_add(size, Relaxed);
            }
        }
        result
    }
}
#[global_allocator]
static ALLOCATOR: Count = Count;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Detail {
    Native(StatusCode),
    Client(K),
    Transport(TransportKind),
}
impl Detail {
    fn kind(self) -> K {
        match self {
            Self::Native(status) => vfsi_core::VfError::from_status(None, status).kind(),
            Self::Client(kind) => kind,
            Self::Transport(TransportKind::Timeout) => K::TimedOut,
            Self::Transport(TransportKind::Auth) => K::PermissionDenied,
            Self::Transport(TransportKind::InvalidReply) => K::InvalidData,
            Self::Transport(_) => K::Other,
        }
    }
}
#[derive(Clone, Debug)]
struct Context {
    operation: Option<&'static str>,
    path: Option<PathBuf>,
    message: Option<Cow<'static, str>>,
    source: Option<Arc<io::Error>>,
}
#[derive(Clone, Debug)]
struct Mini {
    detail: Detail,
    context: Option<Box<Context>>,
}
impl Mini {
    fn kind(&self) -> K {
        self.detail.kind()
    }
    fn status(&self) -> Option<StatusCode> {
        match self.detail {
            Detail::Native(s) => Some(s),
            _ => None,
        }
    }
}
impl fmt::Display for Mini {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.detail)?;
        if let Some(c) = &self.context {
            if let Some(op) = c.operation {
                write!(f, " {op}")?;
            }
            if let Some(p) = &c.path {
                write!(f, " {}", p.display())?;
            }
            if let Some(m) = &c.message {
                write!(f, ": {m}")?;
            }
        }
        Ok(())
    }
}
impl Error for Mini {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.context.as_ref()?.source.as_deref().map(|e| e as _)
    }
}

// Distinct small payloads avoid inflating a bare protocol error with inline
// strings/paths just because a contextual variant exists.
#[derive(Clone, Copy, Debug)]
struct NativePayload(StatusCode);
impl fmt::Display for NativePayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}
impl Error for NativePayload {}
#[derive(Clone, Debug)]
struct TransportPayload(TransportKind, Cow<'static, str>);
impl fmt::Display for TransportPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.0, self.1)
    }
}
impl Error for TransportPayload {}
#[derive(Clone, Debug)]
struct ContextPayload {
    detail: Detail,
    context: Context,
}
impl fmt::Display for ContextPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.detail)?;
        if let Some(op) = self.context.operation {
            write!(f, " {op}")?;
        }
        if let Some(path) = &self.context.path {
            write!(f, " {}", path.display())?;
        }
        if let Some(message) = &self.context.message {
            write!(f, ": {message}")?;
        }
        Ok(())
    }
}

impl Error for ContextPayload {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.context.source.as_deref().map(|e| e as _)
    }
}
#[derive(Clone, Copy)]
enum Spec {
    Client,
    Errno,
    Nfs,
    Smb,
    Transport,
    Context,
}
fn detail(spec: Spec) -> Detail {
    match spec {
        Spec::Client => Detail::Client(K::InvalidInput),
        Spec::Errno => Detail::Native(StatusCode::Errno(libc::EACCES as u32)),
        Spec::Nfs | Spec::Context => Detail::Native(StatusCode::Nfs(13)),
        Spec::Smb => Detail::Native(StatusCode::Smb(0xc000_0022)),
        Spec::Transport => Detail::Transport(TransportKind::Timeout),
    }
}
fn context(source: &Arc<io::Error>) -> Context {
    Context {
        operation: Some("vread"),
        path: Some(PathBuf::from("/cohort/missing")),
        message: Some(Cow::Borrowed("remote request denied")),
        source: Some(source.clone()),
    }
}
fn make_mini(spec: Spec, source: &Arc<io::Error>) -> Mini {
    let context = match spec {
        Spec::Context => Some(Box::new(context(source))),
        Spec::Transport => Some(Box::new(Context {
            operation: None,
            path: None,
            message: Some(Cow::Borrowed("response unavailable")),
            source: None,
        })),
        _ => None,
    };
    Mini {
        detail: detail(spec),
        context,
    }
}
fn make_io(spec: Spec, source: &Arc<io::Error>) -> io::Error {
    let d = detail(spec);
    match spec {
        Spec::Client => K::InvalidInput.into(),
        Spec::Errno => io::Error::from_raw_os_error(libc::EACCES),
        Spec::Nfs | Spec::Smb => {
            let Detail::Native(status) = d else {
                unreachable!()
            };
            io::Error::new(d.kind(), NativePayload(status))
        }
        Spec::Transport => io::Error::new(
            d.kind(),
            TransportPayload(
                TransportKind::Timeout,
                Cow::Borrowed("response unavailable"),
            ),
        ),
        Spec::Context => io::Error::new(
            d.kind(),
            ContextPayload {
                detail: d,
                context: context(source),
            },
        ),
    }
}
fn io_status(error: &io::Error) -> Option<StatusCode> {
    if let Some(code) = error.raw_os_error() {
        return Some(StatusCode::Errno(code as u32));
    }
    let inner = error.get_ref()?;
    if let Some(s) = inner.downcast_ref::<NativePayload>() {
        return Some(s.0);
    }
    if let Some(c) = inner.downcast_ref::<ContextPayload>() {
        return match c.detail {
            Detail::Native(s) => Some(s),
            _ => None,
        };
    }
    None
}
fn io_transport(error: &io::Error) -> Option<TransportKind> {
    error
        .get_ref()?
        .downcast_ref::<TransportPayload>()
        .map(|p| p.0)
}
// This is reconstruction of our known payloads, NOT general io::Error::clone.
// An opaque caller-supplied payload cannot be cloned this way.
fn duplicate_io(error: &io::Error) -> Option<io::Error> {
    if let Some(code) = error.raw_os_error() {
        return Some(io::Error::from_raw_os_error(code));
    }
    match error.get_ref() {
        None => Some(error.kind().into()),
        Some(p) => {
            if let Some(p) = p.downcast_ref::<NativePayload>() {
                Some(io::Error::new(error.kind(), *p))
            } else if let Some(p) = p.downcast_ref::<TransportPayload>() {
                Some(io::Error::new(error.kind(), p.clone()))
            } else {
                p.downcast_ref::<ContextPayload>()
                    .map(|p| io::Error::new(error.kind(), p.clone()))
            }
        }
    }
}

trait Policy {
    type Cause;
    fn make(spec: Spec, source: &Arc<io::Error>) -> Self::Cause;
    fn os(error: io::Error) -> Self::Cause;
    fn duplicate(error: &Self::Cause) -> Self::Cause;
    fn kind(error: &Self::Cause) -> K;
    fn status(error: &Self::Cause) -> Option<StatusCode>;
    fn transport(error: &Self::Cause) -> Option<TransportKind>;
}
struct Minimal;
impl Policy for Minimal {
    type Cause = Mini;
    fn make(spec: Spec, source: &Arc<io::Error>) -> Mini {
        make_mini(spec, source)
    }
    fn os(error: io::Error) -> Mini {
        if let Some(code) = error.raw_os_error() {
            Mini {
                detail: Detail::Native(StatusCode::Errno(code as u32)),
                context: None,
            }
        } else {
            Mini {
                detail: Detail::Client(error.kind()),
                context: Some(Box::new(Context {
                    operation: None,
                    path: None,
                    message: None,
                    source: Some(Arc::new(error)),
                })),
            }
        }
    }
    fn duplicate(error: &Mini) -> Mini {
        error.clone()
    }
    fn kind(error: &Mini) -> K {
        error.kind()
    }
    fn status(error: &Mini) -> Option<StatusCode> {
        error.status()
    }
    fn transport(error: &Mini) -> Option<TransportKind> {
        match error.detail {
            Detail::Transport(kind) => Some(kind),
            _ => None,
        }
    }
}
struct Standard;
impl Policy for Standard {
    type Cause = io::Error;
    fn make(spec: Spec, source: &Arc<io::Error>) -> io::Error {
        make_io(spec, source)
    }
    fn os(error: io::Error) -> io::Error {
        error
    }
    fn duplicate(error: &io::Error) -> io::Error {
        duplicate_io(error).expect("known test payload")
    }
    fn kind(error: &io::Error) -> K {
        error.kind()
    }
    fn status(error: &io::Error) -> Option<StatusCode> {
        io_status(error)
    }
    fn transport(error: &io::Error) -> Option<TransportKind> {
        io_transport(error)
    }
}
struct Shared;
impl Policy for Shared {
    type Cause = Arc<io::Error>;
    fn make(spec: Spec, source: &Arc<io::Error>) -> Self::Cause {
        Arc::new(make_io(spec, source))
    }
    fn os(error: io::Error) -> Self::Cause {
        Arc::new(error)
    }
    fn duplicate(error: &Self::Cause) -> Self::Cause {
        error.clone()
    }
    fn kind(error: &Self::Cause) -> K {
        error.kind()
    }
    fn status(error: &Self::Cause) -> Option<StatusCode> {
        io_status(error)
    }
    fn transport(error: &Self::Cause) -> Option<TransportKind> {
        io_transport(error)
    }
}
#[allow(dead_code)]
struct Outcome<C> {
    cause: C,
    state: FailureState,
    progress: Option<Box<IoProgress>>,
}
#[allow(dead_code)]
struct Indexed<C> {
    index: Option<usize>,
    cause: C,
}
fn failure<C>(cause: C) -> Outcome<C> {
    Outcome {
        cause,
        state: FailureState::Failed,
        progress: None,
    }
}
fn layouts<C>() -> serde_json::Value {
    serde_json::json!({
        "cause":size_of::<C>(), "op_error":size_of::<Outcome<C>>(),
        "indexed_error":size_of::<Indexed<C>>(),
        "op_result_unit":size_of::<Result<(),Outcome<C>>>(),
        "op_result_usize":size_of::<Result<usize,Outcome<C>>>(),
        "op_result_read":size_of::<Result<ReadResult,Outcome<C>>>(),
        "op_result_attrs":size_of::<Result<Attrs,Outcome<C>>>(),
        "outer_read":size_of::<Result<Vec<Result<ReadResult,Outcome<C>>>,C>>()
    })
}

const N: usize = 64;
const LENGTH: usize = 4096;
struct RingReads {
    ring: io_uring::IoUring,
    files: Vec<File>,
    buffers: Vec<Vec<u8>>,
    poisoned: bool,
    submitted: u64,
    completed: u64,
    waves: u64,
}
impl RingReads {
    fn new(root: &Path, mixed: bool) -> io::Result<Self> {
        let files = open_files(root, mixed)?;
        let buffers = (0..N).map(|_| vec![0; LENGTH]).collect();
        let ring = io_uring::IoUring::new(256)?;
        Ok(Self {
            ring,
            files,
            buffers,
            poisoned: false,
            submitted: 0,
            completed: 0,
            waves: 0,
        })
    }
    fn wave(&mut self) -> io::Result<Vec<i32>> {
        self.poisoned = true;
        for i in 0..N {
            let sqe = io_uring::opcode::Read::new(
                io_uring::types::Fd(self.files[i].as_raw_fd()),
                self.buffers[i].as_mut_ptr(),
                LENGTH as u32,
            )
            .offset(0)
            .build()
            .user_data(i as u64);
            // SAFETY: queue capacity exceeds N; owned files and stable buffers
            // remain live through every CQE or are retained on a fatal error.
            unsafe { self.ring.submission().push(&sqe) }.expect("bounded queue");
        }
        self.submitted += N as u64;
        self.waves += 1;
        let mut result = vec![i32::MIN; N];
        let mut done = 0;
        while done < N {
            match self.ring.submit_and_wait(N - done) {
                Err(e) if e.kind() == K::Interrupted => continue,
                Err(e) => return Err(e),
                Ok(_) => {}
            }
            for cqe in self.ring.completion() {
                let index = cqe.user_data() as usize;
                assert!(index < N && result[index] == i32::MIN, "invalid CQE");
                result[index] = cqe.result();
                done += 1;
                self.completed += 1;
            }
        }
        self.poisoned = false;
        Ok(result)
    }
}
impl Drop for RingReads {
    fn drop(&mut self) {
        if self.poisoned {
            // A fatal ring error must not free memory still borrowed by kernel I/O.
            std::mem::forget(std::mem::take(&mut self.buffers));
            std::mem::forget(std::mem::take(&mut self.files));
        }
    }
}
fn open_files(root: &Path, mixed: bool) -> io::Result<Vec<File>> {
    (0..N)
        .map(|i| {
            File::open(root.join(if mixed && i == N / 2 {
                "directory".into()
            } else {
                format!("file-{i}")
            }))
        })
        .collect()
}
fn cpu_ns() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the supplied storage on success.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let u = unsafe { usage.assume_init() };
    ((u.ru_utime.tv_sec + u.ru_stime.tv_sec) as u64) * 1_000_000_000
        + ((u.ru_utime.tv_usec + u.ru_stime.tv_usec) as u64) * 1000
}
fn run<P: Policy>(scenario: &str, iterations: usize, fixture: &Path) -> serde_json::Value {
    let source = Arc::new(io::Error::other("original source diagnostic"));
    let spec = if scenario.ends_with("context") {
        Spec::Context
    } else if scenario.ends_with("transport") {
        Spec::Transport
    } else if scenario.ends_with("smb") {
        Spec::Smb
    } else if scenario.ends_with("nfs") {
        Spec::Nfs
    } else if scenario.ends_with("client") {
        Spec::Client
    } else {
        Spec::Errno
    };
    let original = P::make(spec, &source);
    let mixed = scenario.ends_with("mixed");
    let files = scenario
        .starts_with("posix")
        .then(|| open_files(fixture, mixed).unwrap());
    let mut buffers: Vec<Vec<u8>> = (0..N).map(|_| vec![0; LENGTH]).collect();
    let mut ring = scenario
        .starts_with("ring")
        .then(|| RingReads::new(fixture, mixed).unwrap());
    let perform =
        |count: usize, ring: &mut Option<RingReads>, buffers: &mut [Vec<u8>]| -> (u64, u64, u64) {
            let mut successful = 0;
            let mut failed = 0;
            let mut bytes = 0;
            for _ in 0..count {
                if scenario.starts_with("inspect") {
                    black_box(P::status(black_box(&original)));
                    black_box(P::transport(black_box(&original)));
                    black_box(P::kind(black_box(&original)));
                } else if scenario.starts_with("construct") || scenario.starts_with("clone") {
                    let cause = if scenario.starts_with("clone") {
                        P::duplicate(black_box(&original))
                    } else {
                        P::make(black_box(spec), black_box(&source))
                    };
                    black_box(P::kind(&cause));
                    drop(black_box(cause));
                } else if scenario.starts_with("fanout") || scenario.starts_with("void") {
                    let outcomes: Vec<Result<(), Outcome<P::Cause>>> = (0..N)
                        .map(|_| {
                            if scenario.starts_with("void") {
                                Ok(())
                            } else {
                                Err(failure(P::duplicate(black_box(&original))))
                            }
                        })
                        .collect();
                    for outcome in &outcomes {
                        if outcome.is_err() {
                            failed += 1;
                        } else {
                            successful += 1;
                        }
                    }
                    drop(black_box(outcomes));
                } else {
                    // Real reads and synthetic completions use exactly the same
                    // final ReadResult/error envelope assembly for both policies.
                    let completions: Vec<i32> = if let Some(ring) = ring {
                        ring.wave().expect("ring wave")
                    } else if let Some(files) = &files {
                        files
                            .iter()
                            .zip(buffers.iter_mut())
                            .map(|(f, b)| match f.read_at(b, 0) {
                                Ok(n) => n as i32,
                                Err(e) => -e.raw_os_error().expect("OS errno"),
                            })
                            .collect()
                    } else {
                        (0..N)
                            .map(|i| {
                                let error = scenario.contains("all")
                                    || (scenario.contains("mixed") && i == N / 2);
                                if error { -libc::EISDIR } else { LENGTH as i32 }
                            })
                            .collect()
                    };
                    let outcomes: Vec<Result<ReadResult, Outcome<P::Cause>>> = completions
                        .into_iter()
                        .map(|n| {
                            if n >= 0 {
                                Ok(ReadResult::buffered(0, n as usize, false))
                            } else {
                                let cause =
                                    if scenario.starts_with("vec") && scenario.ends_with("nfs") {
                                        P::make(Spec::Nfs, &source)
                                    } else {
                                        P::os(io::Error::from_raw_os_error(-n))
                                    };
                                Err(failure(cause))
                            }
                        })
                        .collect();
                    for outcome in &outcomes {
                        match outcome {
                            Ok(r) => {
                                successful += 1;
                                bytes += r.bytes_read() as u64;
                            }
                            Err(e) => {
                                failed += 1;
                                black_box(P::kind(&e.cause));
                            }
                        }
                    }
                    drop(black_box(outcomes));
                }
            }
            (successful, failed, bytes)
        };
    perform(10, &mut ring, &mut buffers);
    if let Some(r) = &mut ring {
        r.submitted = 0;
        r.completed = 0;
        r.waves = 0;
    }
    let start_live = LIVE.load(Relaxed);
    PEAK.store(start_live, Relaxed);
    ALLOCS.store(0, Relaxed);
    BYTES.store(0, Relaxed);
    let start_cpu = cpu_ns();
    let start = Instant::now();
    TRACK.store(true, Relaxed);
    let (successful, failed, bytes) = perform(iterations, &mut ring, &mut buffers);
    TRACK.store(false, Relaxed);
    let wall_ns = start.elapsed().as_nanos() as u64;
    let cpu = cpu_ns() - start_cpu;
    let allocations = ALLOCS.load(Relaxed);
    let allocated_bytes = BYTES.load(Relaxed);
    let peak = PEAK.load(Relaxed).saturating_sub(start_live);
    let retained = LIVE.load(Relaxed).saturating_sub(start_live);
    assert_eq!(retained, 0, "measured workload leaked memory");
    let io_counts = if let Some(r) = ring {
        serde_json::json!({"sqes":r.submitted,"cqes":r.completed,"waves":r.waves})
    } else if files.is_some() {
        serde_json::json!({"pread_calls":iterations*N})
    } else {
        serde_json::Value::Null
    };
    serde_json::json!({"scenario":scenario,"iterations":iterations,"warmup_iterations":10,
        "wall_ns":wall_ns,"cpu_ns":cpu,"ns_per_iteration":wall_ns as f64/iterations as f64,
        "allocations":allocations,"allocated_bytes":allocated_bytes,
        "allocations_per_iteration":allocations as f64/iterations as f64,
        "bytes_per_iteration":allocated_bytes as f64/iterations as f64,
        "peak_extra_live_bytes":peak,"retained_extra_bytes":retained,
        "successful":successful,"failed":failed,"returned_bytes":bytes,
        "io":io_counts,"allocation_instrumented":cfg!(feature="allocation-stats")})
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("layout") {
        println!(
            "{}",
            serde_json::json!({"minimal":layouts::<Mini>(),
            "io":layouts::<io::Error>(),"arc_io":layouts::<Arc<io::Error>>(),
            "read_result":size_of::<ReadResult>(),"attrs":size_of::<Attrs>(),
            "native_payload":size_of::<NativePayload>(),"context":size_of::<Context>(),
            "current_vferror":size_of::<vfsi_core::VfError>(),
            "current_op_error":size_of::<vfsi_core::OpError>(),
            "current_op_result_unit":size_of::<vfsi_core::OpResult<()>>(),
            "current_op_result_read":size_of::<vfsi_core::OpResult<ReadResult>>(),
            "compiled_rustc":env!("VFERROR_COMPILED_RUSTC")})
        );
        return;
    }
    let repr = &args[1];
    let scenario = &args[2];
    let iterations = args[3].parse().unwrap();
    let fixture = Path::new(&args[4]);
    let result = match repr.as_str() {
        "minimal" => run::<Minimal>(scenario, iterations, fixture),
        "io" => run::<Standard>(scenario, iterations, fixture),
        "arc_io" => run::<Shared>(scenario, iterations, fixture),
        _ => panic!("unknown policy"),
    };
    println!("{result}");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portable_categories_and_native_namespaces_survive() {
        let source = Arc::new(io::Error::other("original diagnostic"));
        for spec in [
            Spec::Client,
            Spec::Errno,
            Spec::Nfs,
            Spec::Smb,
            Spec::Transport,
            Spec::Context,
        ] {
            let mini = make_mini(spec, &source);
            let io = make_io(spec, &source);
            assert_eq!(mini.kind(), io.kind());
            assert_eq!(mini.status(), io_status(&io));
            let clone = duplicate_io(&io).unwrap();
            assert_eq!(io.kind(), clone.kind());
            assert_eq!(io_status(&io), io_status(&clone));
        }
        assert_eq!(
            io_status(&make_io(Spec::Nfs, &source)),
            Some(StatusCode::Nfs(13))
        );
        assert_eq!(
            io_status(&make_io(Spec::Smb, &source)),
            Some(StatusCode::Smb(0xc000_0022))
        );
        assert_eq!(
            io_transport(&make_io(Spec::Transport, &source)),
            Some(TransportKind::Timeout)
        );
    }
    #[test]
    fn contextual_source_and_path_survive_reconstruction() {
        let source = Arc::new(io::Error::other("original diagnostic"));
        let mini = make_mini(Spec::Context, &source);
        let io = make_io(Spec::Context, &source);
        let clone = duplicate_io(&io).unwrap();
        for e in [&io, &clone] {
            assert!(e.to_string().contains("/cohort/missing"));
            assert_eq!(e.source().unwrap().to_string(), source.to_string());
            let p = e
                .get_ref()
                .unwrap()
                .downcast_ref::<ContextPayload>()
                .unwrap();
            assert_eq!(
                p.context.path.as_deref(),
                Some(Path::new("/cohort/missing"))
            );
            assert!(Arc::ptr_eq(p.context.source.as_ref().unwrap(), &source));
        }
        assert!(Arc::ptr_eq(
            mini.context.unwrap().source.as_ref().unwrap(),
            &source
        ));
    }
    #[test]
    fn arbitrary_io_payload_is_not_silently_cloned_or_dropped() {
        #[derive(Debug)]
        struct Opaque;
        impl fmt::Display for Opaque {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("opaque")
            }
        }
        impl Error for Opaque {}
        let original = io::Error::other(Opaque);
        assert!(duplicate_io(&original).is_none());
        let a = Arc::new(original);
        let b = a.clone();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(b.get_ref().unwrap().is::<Opaque>());
    }
    #[test]
    fn certainty_progress_and_attribution_are_separate_from_cause() {
        let source = Arc::new(io::Error::other("source"));
        for state in [
            FailureState::NotAttempted,
            FailureState::Failed,
            FailureState::Unknown,
        ] {
            let error = Outcome {
                cause: make_io(Spec::Transport, &source),
                state,
                progress: (state != FailureState::NotAttempted).then(|| {
                    Box::new(IoProgress::Read(ReadResult::owned(8, vec![1, 2, 3], false)))
                }),
            };
            assert_eq!(error.state, state);
            assert_eq!(error.cause.kind(), K::TimedOut);
            if let Some(p) = error.progress {
                let IoProgress::Read(p) = *p else { panic!() };
                assert_eq!(p.offset(), 8);
                assert_eq!(p.data(), Some([1, 2, 3].as_slice()));
            }
        }
        let item = Indexed {
            index: Some(3),
            cause: make_io(Spec::Nfs, &source),
        };
        assert_eq!(item.index, Some(3));
        assert_eq!(io_status(&item.cause), Some(StatusCode::Nfs(13)));
    }
    #[test]
    fn real_posix_and_ring_shapes_preserve_work_and_errors() {
        let root = std::env::var("VFERROR_BENCH_FIXTURE").expect("required owned fixture");
        for scenario in ["posix_ok", "posix_mixed", "ring_ok", "ring_mixed"] {
            let a = run::<Minimal>(scenario, 1, Path::new(&root));
            let b = run::<Standard>(scenario, 1, Path::new(&root));
            for field in ["successful", "failed", "returned_bytes", "io"] {
                assert_eq!(a[field], b[field]);
            }
            assert_eq!(a["failed"], if scenario.ends_with("mixed") { 1 } else { 0 });
        }
    }
}
