//! Opt-in measurement of Rust allocation requests at the C listing boundary.
use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static REQUESTS: Cell<(bool, usize, usize)> = const { Cell::new((false, 0, 0)) };
}

struct CountingAllocator;
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn requested(bytes: usize) {
    let _ = REQUESTS.try_with(|counts| {
        let (enabled, allocations, total) = counts.get();
        if enabled {
            counts.set((true, allocations + 1, total + bytes));
        }
    });
}

// SAFETY: allocation and deallocation are forwarded unchanged to System.
// Only this thread's successful allocation requests are counted.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: GlobalAlloc's caller supplies the valid allocation layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            requested(layout.size());
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same allocator and valid layout, with zeroing preserved.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            requested(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the pointer/layout belong to the forwarded System allocation.
        unsafe { System.dealloc(ptr, layout) };
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: forward the pointer/layout and valid requested size unchanged.
        let ptr = unsafe { System.realloc(ptr, layout, size) };
        if !ptr.is_null() {
            requested(size);
        }
        ptr
    }
}

#[test]
#[ignore = "manual C snapshot allocation measurement; run explicitly with --ignored --exact"]
fn indexed_listing_allocation_profile() {
    let root = tempfile::tempdir().unwrap();
    let dirs: Vec<_> = (0..32)
        .map(|index| {
            let name = format!("directory-{index:02}-{}", "d".repeat(80));
            let path = root.path().join(&name);
            std::fs::create_dir(&path).unwrap();
            for entry in 0..32 {
                std::fs::write(
                    path.join(format!("file-{entry:02}-{}", "f".repeat(32))),
                    b"x",
                )
                .unwrap();
            }
            CString::new(format!("/{name}")).unwrap()
        })
        .collect();
    let pointers: Vec<_> = dirs.iter().map(|dir| dir.as_ptr()).collect();
    let backend = vfsi_posix::backend(root.path()).unwrap();
    let fs = make_fs(Box::new(backend), PathBuf::from("/"), PathBuf::from("/"));
    let options = vfsi_listing_options {
        max_entries: usize::MAX,
        max_path_bytes: usize::MAX,
        attributes: VFSI_ATTR_MODE | VFSI_ATTR_SIZE,
    };
    let mut results = vec![vfsi_listing_result::default(); dirs.len()];
    let mut delivered = 0usize;
    unsafe extern "C" fn count(
        _: usize,
        _: *const c_char,
        _: *const c_char,
        _: *const vfsi_attrs,
        data: *mut c_void,
    ) -> bool {
        // SAFETY: data points to the exclusive count for the synchronous call.
        unsafe { *(data as *mut usize) += 1 };
        true
    }
    REQUESTS.with(|counts| counts.set((true, 0, 0)));
    // SAFETY: filesystem, arrays, callback count, and strings stay live and exclusive.
    let status = unsafe {
        vfsi_listdirs(
            fs,
            pointers.as_ptr(),
            pointers.len(),
            &options,
            Some(count),
            (&mut delivered as *mut usize).cast(),
            results.as_mut_ptr(),
        )
    };
    let (_, allocations, bytes) = REQUESTS.with(|counts| counts.replace((false, 0, 0)));
    // SAFETY: this is the live constructor-owned handle after callbacks finished.
    unsafe { vfsi_free(fs) };
    assert_eq!(status, 0);
    assert_eq!(delivered, 1024);
    assert!(results
        .iter()
        .all(|result| result.completion == 1 && result.delivered == 32));
    println!("LISTING_ALLOCATIONS {{\"entries\":1024,\"allocation_requests\":{allocations},\"requested_bytes\":{bytes}}}");
}
