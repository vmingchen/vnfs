//! Compare facade adapters with the existing backend path, including allocations.
#![cfg(target_os = "linux")]
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use vnfs::FsExt;
use vnfs::{Mounted, OpenFlags, OpenRequest};

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    static COUNTS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}
struct CountingAllocator;
fn record(bytes: usize) {
    let _ = TRACK.try_with(|track| {
        if track.get() {
            let _ = COUNTS.try_with(|counts| {
                let (calls, total) = counts.get();
                counts.set((calls + 1, total + bytes));
            });
        }
    });
}
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(pointer, layout, size) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
fn measured<T>(f: impl FnOnce() -> T) -> (T, (usize, usize)) {
    COUNTS.with(|counts| counts.set((0, 0)));
    TRACK.with(|track| track.set(true));
    let value = f();
    TRACK.with(|track| track.set(false));
    (value, COUNTS.with(Cell::get))
}

#[test]
fn auto_range_only_reads_keep_the_legacy_routing_allocation_cost() {
    let root = tempfile::tempdir().unwrap();
    let auto = vnfs::Auto::new(root.path()).unwrap();
    let mounted = Mounted::new(root.path()).unwrap();
    mounted
        .write_files(&[("/a", b"abc"), ("/b", b"def")])
        .unwrap();
    let paths = ["/a", "/b"];
    let auto_files = auto.open_options().read(true).openv(&paths).unwrap();
    let files = mounted.open_options().read(true).openv(&paths).unwrap();
    let auto_reads: Vec<_> = auto_files
        .iter()
        .map(|file| vnfs::ReadOp::range(file, 0, 3))
        .collect();
    let reads: Vec<_> = files
        .iter()
        .map(|file| vnfs::ReadOp::range(file, 0, 3))
        .collect();
    let (actual, cost) = measured(|| auto.readv(auto_reads).unwrap());
    let (expected, baseline) = measured(|| mounted.readv(reads).unwrap());
    assert_eq!(actual, expected);
    // Legacy Auto routing needs only a result vector and one backend request
    // vector beyond Mounted. Partition/index/scatter vectors are unnecessary.
    let routing_bytes = paths.len()
        * (std::mem::size_of::<vnfs::ReadResult>()
            + std::mem::size_of::<vnfs::backend::FsRead<'_, vnfs::backend::DummyVecFs>>());
    assert_eq!(cost.0, baseline.0 + 2);
    assert!(cost.1 <= baseline.1 + routing_bytes);
}

#[test]
fn unified_operation_construction_does_not_allocate() {
    let temp = tempfile::tempdir().unwrap();
    let fs = Mounted::new(temp.path()).unwrap();
    fs.write("/a", b"abc").unwrap();
    let file = fs.open("/a").unwrap();
    let mut buffer = [0; 1];
    let (ops, cost) = measured(|| {
        [
            vnfs::ReadOp::whole("/a"),
            vnfs::ReadOp::range(&file, 0, 1),
            vnfs::ReadOp::into(&file, 0, &mut buffer),
        ]
    });
    assert_eq!(cost, (0, 0));
    let results = fs.readv(ops).unwrap();
    assert_eq!(results[2].data, None);
    assert_eq!(&buffer, b"a");
}

#[test]
fn opaque_adapters_preserve_batch_allocations_and_borrowed_storage() {
    use vnfs::backend::{DummyVecFs, FsClient};
    let root = tempfile::TempDir::new().unwrap();
    let mounted = Mounted::new(root.path()).unwrap();
    let raw = FsClient::new(DummyVecFs::try_new(root.path().to_path_buf()).unwrap());
    let paths = ["/a", "/b", "/c"];
    mounted
        .write_files(&paths.map(|path| (path, b"payload")))
        .unwrap();
    // Both clients must have warmed descriptor maps before comparing OPEN.
    raw.write_files(&paths.map(|path| (path, b"payload")))
        .unwrap();
    let requests = paths.map(|path| OpenRequest::new(path, OpenFlags::READ | OpenFlags::WRITE));
    let (mut files, open_cost) = measured(|| mounted.openv(&requests).unwrap());
    let (mut raw_files, raw_open_cost) = measured(|| raw.openv(&requests).unwrap());
    assert_eq!(
        open_cost, raw_open_cost,
        "opaque OPEN must reuse the result allocation"
    );
    let reads: Vec<_> = files.iter().map(|f| vnfs::ReadOp::range(f, 0, 7)).collect();
    let raw_reads: Vec<_> = raw_files
        .iter()
        .map(|f| vnfs::ReadOp::range(f, 0, 7))
        .collect();
    let (results, cost) = measured(|| mounted.readv(reads).unwrap());
    let (expected, raw_cost) = measured(|| vnfs::Fs::readv(&raw, raw_reads).unwrap());
    assert_eq!(results, expected);
    assert_eq!(
        cost, raw_cost,
        "READ projection must not allocate another request vector"
    );
    let writes: Vec<_> = files
        .iter()
        .map(|f| vnfs::WriteOp::at(f, 0, b"payload"))
        .collect();
    let raw_writes: Vec<_> = raw_files
        .iter()
        .map(|f| f.write_request_at(0, b"payload"))
        .collect();
    let (results, cost) = measured(|| mounted.write_allv(&writes).unwrap());
    let (expected, raw_cost) = measured(|| raw.write_allv(&raw_writes).unwrap());
    assert_eq!(results, expected);
    assert_eq!(
        cost, raw_cost,
        "WRITE waves must not allocate facade request vectors"
    );
    drop(writes);
    drop(raw_writes);

    let ((), cost) = measured(|| mounted.try_closev(&mut files).unwrap());
    let ((), raw_cost) = measured(|| raw.try_closev(&mut raw_files).unwrap());
    assert_eq!(cost, raw_cost);
    assert!(files.iter().all(|file| file.is_closed()));
    let mut buffers = [[0; 7]; 3];
    let mut raw_buffers = [[0; 7]; 3];
    let reopened = mounted.openv(&requests).unwrap();
    let raw_reopened = raw.openv(&requests).unwrap();
    let requests: Vec<_> = reopened
        .iter()
        .zip(&mut buffers)
        .map(|(file, buffer)| vnfs::ReadOp::into(file, 0, buffer))
        .collect();
    let raw_requests: Vec<_> = raw_reopened
        .iter()
        .zip(&mut raw_buffers)
        .map(|(file, buffer)| vnfs::ReadOp::into(file, 0, buffer))
        .collect();
    let (results, cost) = measured(|| mounted.readv(requests).unwrap());
    let (expected, raw_cost) = measured(|| vnfs::Fs::readv(&raw, raw_requests).unwrap());
    assert_eq!(results, expected);
    assert_eq!(
        cost, raw_cost,
        "READ_INTO projection must not allocate another request vector"
    );
    assert!(buffers.iter().all(|buffer| buffer == b"payload"));
    mounted.closev(reopened).unwrap();
    raw.closev(raw_reopened).unwrap();
    mounted.remove_paths(&paths, false).unwrap();
}

#[test]
fn opaque_requests_preserve_owner_preflight_and_error_sources() {
    let root = tempfile::TempDir::new().unwrap();
    let owner = Mounted::new(root.path()).unwrap();
    let other = Mounted::new(root.path()).unwrap();
    owner.write("/a", b"original").unwrap();
    let mut file = owner
        .open_options()
        .read(true)
        .write(true)
        .open("/a")
        .unwrap();
    let (request, allocations) = measured(|| vnfs::WriteOp::at(&file, 0, b"changed"));
    assert_eq!(allocations, (0, 0));
    assert!(other.writev(&[request]).is_err());
    assert_eq!(
        owner
            .readv_with_options([vnfs::ReadOp::whole("/a")], vnfs::ReadOptions::default())
            .unwrap()[0]
            .data
            .as_deref()
            .unwrap(),
        b"original"
    );
    assert!(other.try_closev([&mut file]).is_err());
    assert!(!file.is_closed());
    owner.try_closev([&mut file]).unwrap();
    assert!(file.is_closed());
    let error = std::io::Read::read(&mut file, &mut [0; 1]).unwrap_err();
    assert!(
        error
            .get_ref()
            .unwrap()
            .downcast_ref::<vnfs::Error>()
            .is_some()
    );
    owner.remove_file("/a").unwrap();
}

#[test]
fn handles_remain_send_sync_and_clients_remain_cheaply_cloneable() {
    fn send_sync<T: Send + Sync>() {}
    fn cloneable<T: Clone>() {}
    send_sync::<vnfs::NfsClient>();
    send_sync::<vnfs::NfsFile>();
    send_sync::<vnfs::NfsDir>();
    send_sync::<vnfs::Mounted>();
    send_sync::<vnfs::MountedFile>();
    cloneable::<vnfs::NfsClient>();
    cloneable::<vnfs::Mounted>();
    let root = tempfile::TempDir::new().unwrap();
    let client = Mounted::new(root.path()).unwrap();
    let (clone, allocations) = measured(|| client.clone());
    assert_eq!(allocations, (0, 0), "client clone must share its backend");
    client.write("/shared", b"data").unwrap();
    let mut file = client.open("/shared").unwrap();
    clone.try_closev([&mut file]).unwrap();
    assert!(
        file.is_closed(),
        "clones must retain the same ownership identity"
    );
    assert_eq!(
        std::mem::size_of::<vnfs::NfsFile>(),
        std::mem::size_of::<vnfs::backend::FsFile<vnfs::backend::NfsVecFs>>()
    );
}
