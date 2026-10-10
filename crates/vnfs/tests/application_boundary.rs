//! Compare facade adapters with the existing backend path, including allocations.
#![cfg(target_os = "linux")]
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use vnfs::{OpenFlags, OpenOp, Posix};
use vnfs::{Vfsi, VfsiExt};

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
    let mounted = Posix::new(root.path()).unwrap();
    mounted
        .write_files(&[("/a", b"abc"), ("/b", b"def")])
        .unwrap();
    let paths = ["/a", "/b"];
    let auto_files = auto.open_options().read(true).vopen(&paths).unwrap();
    let files = mounted.open_options().read(true).vopen(&paths).unwrap();
    let auto_reads: Vec<_> = auto_files
        .iter()
        .map(|file| vnfs::ReadOp::range(file, 0, 3))
        .collect();
    let reads: Vec<_> = files
        .iter()
        .map(|file| vnfs::ReadOp::range(file, 0, 3))
        .collect();
    let (actual, cost) = measured(|| auto.vread(auto_reads, Default::default()).unwrap());
    let (expected, baseline) = measured(|| mounted.vread(reads, Default::default()).unwrap());
    assert_eq!(actual, expected);
    // Legacy Auto routing needs only a result vector and one backend request
    // vector beyond Posix. Partition/index/scatter vectors are unnecessary.
    let routing_bytes = paths.len()
        * (std::mem::size_of::<vnfs::ReadResult>()
            + std::mem::size_of::<vfsi_sync::FsRead<'_, vfsi_local::LocalBackend>>());
    assert_eq!(cost.0, baseline.0 + 2);
    assert!(cost.1 <= baseline.1 + routing_bytes);
}

#[test]
fn unified_operation_construction_does_not_allocate() {
    let temp = tempfile::tempdir().unwrap();
    let fs = Posix::new(temp.path()).unwrap();
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
    let results = fs.vread(ops, Default::default()).unwrap();
    assert_eq!(results[2].data(), None);
    assert_eq!(&buffer, b"a");
}

#[test]
fn opaque_adapters_preserve_batch_allocations_and_borrowed_storage() {
    use vfsi_sync::FsClient;
    let root = tempfile::TempDir::new().unwrap();
    let mounted = Posix::new(root.path()).unwrap();
    let raw = FsClient::new(vfsi_posix::backend(root.path()).unwrap());
    let paths = ["/a", "/b", "/c"];
    mounted
        .write_files(&paths.map(|path| (path, b"payload")))
        .unwrap();
    // Both clients must have warmed descriptor maps before comparing OPEN.
    raw.write_files(&paths.map(|path| (path, b"payload")))
        .unwrap();
    let requests = paths.map(|path| OpenOp::new(path, OpenFlags::READ | OpenFlags::WRITE));
    let (mut files, open_cost) = measured(|| mounted.vopen(&requests).unwrap());
    let (mut raw_files, raw_open_cost) = measured(|| raw.vopen(&requests).unwrap());
    assert_eq!(
        open_cost, raw_open_cost,
        "opaque OPEN must reuse the result allocation"
    );
    let reads: Vec<_> = files.iter().map(|f| vnfs::ReadOp::range(f, 0, 7)).collect();
    let raw_reads: Vec<_> = raw_files
        .iter()
        .map(|f| vnfs::ReadOp::range(f, 0, 7))
        .collect();
    let (results, cost) = measured(|| mounted.vread(reads, Default::default()).unwrap());
    let (expected, raw_cost) =
        measured(|| vnfs::Vfsi::vread(&raw, raw_reads, Default::default()).unwrap());
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
        .map(|f| vnfs::WriteOp::at(f, 0, b"payload"))
        .collect();
    let (results, cost) = measured(|| {
        mounted
            .vwrite(&writes, vnfs::WriteOptions::new().write_all(true))
            .unwrap()
    });
    let (expected, raw_cost) = measured(|| {
        raw.vwrite(&raw_writes, vnfs::WriteOptions::new().write_all(true))
            .unwrap()
    });
    assert_eq!(results, expected);
    assert_eq!(
        cost, raw_cost,
        "WRITE waves must not allocate facade request vectors"
    );
    drop(writes);
    drop(raw_writes);

    let ((), cost) = measured(|| mounted.vclose(&mut files).unwrap());
    let ((), raw_cost) = measured(|| raw.vclose(&mut raw_files).unwrap());
    assert_eq!(cost, raw_cost);
    assert!(files.iter().all(|file| file.is_closed()));
    let mut buffers = [[0; 7]; 3];
    let mut raw_buffers = [[0; 7]; 3];
    let reopened = mounted.vopen(&requests).unwrap();
    let raw_reopened = raw.vopen(&requests).unwrap();
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
    let (results, cost) = measured(|| mounted.vread(requests, Default::default()).unwrap());
    let (expected, raw_cost) =
        measured(|| vnfs::Vfsi::vread(&raw, raw_requests, Default::default()).unwrap());
    assert_eq!(results, expected);
    assert_eq!(
        cost, raw_cost,
        "READ_INTO projection must not allocate another request vector"
    );
    assert!(buffers.iter().all(|buffer| buffer == b"payload"));
    mounted.close_files(reopened).unwrap();
    raw.vclose_owned(raw_reopened).unwrap();
    mounted
        .vremove(&paths, vnfs::RemoveMode::Entry, Default::default())
        .unwrap();
}

#[test]
fn opaque_requests_preserve_owner_preflight_and_error_sources() {
    let root = tempfile::TempDir::new().unwrap();
    let owner = Posix::new(root.path()).unwrap();
    let other = Posix::new(root.path()).unwrap();
    owner.write("/a", b"original").unwrap();
    let mut file = owner
        .open_options()
        .read(true)
        .write(true)
        .open("/a")
        .unwrap();
    let (request, allocations) = measured(|| vnfs::WriteOp::at(&file, 0, b"changed"));
    assert_eq!(allocations, (0, 0));
    assert!(other.vwrite(&[request], Default::default()).is_err());
    assert_eq!(
        owner
            .vread([vnfs::ReadOp::whole("/a")], vnfs::ReadOptions::default())
            .unwrap()[0]
            .data()
            .unwrap(),
        b"original"
    );
    assert!(other.vclose(std::slice::from_mut(&mut file)).is_err());
    assert!(!file.is_closed());
    owner.vclose(std::slice::from_mut(&mut file)).unwrap();
    assert!(file.is_closed());
    let error = std::io::Read::read(&mut owner.std_io(&file), &mut [0; 1]).unwrap_err();
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
    send_sync::<vnfs::Posix>();
    send_sync::<vnfs::PosixFile>();
    cloneable::<vnfs::NfsClient>();
    cloneable::<vnfs::Posix>();
    let root = tempfile::TempDir::new().unwrap();
    let client = Posix::new(root.path()).unwrap();
    let (clone, allocations) = measured(|| client.clone());
    assert_eq!(allocations, (0, 0), "client clone must share its backend");
    client.write("/shared", b"data").unwrap();
    let mut file = client.open("/shared").unwrap();
    clone.vclose(std::slice::from_mut(&mut file)).unwrap();
    assert!(
        file.is_closed(),
        "clones must retain the same ownership identity"
    );
    assert_eq!(
        std::mem::size_of::<vnfs::NfsFile>(),
        std::mem::size_of::<vfsi_sync::FsFile<vfsi_nfs::NfsVecFs>>()
    );
}
