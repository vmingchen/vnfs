//! Compare facade adapters with the existing backend path, including allocations.
#![cfg(target_os = "linux")]
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
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
    let reads: Vec<_> = files.iter().map(|f| f.read_request_at(0, 7)).collect();
    let raw_reads: Vec<_> = raw_files.iter().map(|f| f.read_request_at(0, 7)).collect();
    let (results, cost) = measured(|| mounted.readv(&reads).unwrap());
    let (expected, raw_cost) = measured(|| raw.readv(&raw_reads).unwrap());
    assert_eq!(results, expected);
    assert_eq!(
        cost, raw_cost,
        "READ projection must not allocate another request vector"
    );
    let writes: Vec<_> = files
        .iter()
        .map(|f| f.write_request_at(0, b"payload"))
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
    drop(reads);
    drop(raw_reads);
    let ((), cost) = measured(|| mounted.try_closev(&mut files).unwrap());
    let ((), raw_cost) = measured(|| raw.try_closev(&mut raw_files).unwrap());
    assert_eq!(cost, raw_cost);
    assert!(files.iter().all(|file| file.is_closed()));
    let mut buffers = [[0; 7]; 3];
    let mut raw_buffers = [[0; 7]; 3];
    let reopened = mounted.openv(&requests).unwrap();
    let raw_reopened = raw.openv(&requests).unwrap();
    let mut requests: Vec<_> = reopened
        .iter()
        .zip(&mut buffers)
        .map(|(file, buffer)| file.read_request_at_into(0, buffer))
        .collect();
    let mut raw_requests: Vec<_> = raw_reopened
        .iter()
        .zip(&mut raw_buffers)
        .map(|(file, buffer)| file.read_request_at_into(0, buffer))
        .collect();
    let (results, cost) = measured(|| mounted.readv_into(&mut requests).unwrap());
    let (expected, raw_cost) = measured(|| raw.readv_into(&mut raw_requests).unwrap());
    assert_eq!(results, expected);
    assert_eq!(
        cost, raw_cost,
        "READ_INTO projection must not allocate another request vector"
    );
    drop(requests);
    drop(raw_requests);
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
    let (request, allocations) = measured(|| file.write_request_at(0, b"changed"));
    assert_eq!(allocations, (0, 0));
    assert!(other.writev(&[request]).is_err());
    assert_eq!(owner.read("/a").unwrap(), b"original");
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
