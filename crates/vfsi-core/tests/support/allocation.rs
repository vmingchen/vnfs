use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

struct TrackingAllocator;
thread_local! {
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
    static ALLOCATED_BYTES: Cell<Option<usize>> = const { Cell::new(None) };
}
#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;
// SAFETY: pointer/layout contracts are delegated unchanged to System; tracking uses only TLS cells.
unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|counter| {
            if let Some(count) = counter.get() {
                counter.set(Some(count + 1));
            }
        });
        let _ = ALLOCATED_BYTES.try_with(|counter| {
            if let Some(bytes) = counter.get() {
                counter.set(Some(bytes + layout.size()));
            }
        });
        // SAFETY: forward the allocator contract unchanged to the system.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: pointer and layout come from the same delegated allocator.
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|counter| {
            if let Some(count) = counter.get() {
                counter.set(Some(count + 1));
            }
        });
        let _ = ALLOCATED_BYTES.try_with(|counter| {
            if let Some(bytes) = counter.get() {
                counter.set(Some(bytes + size));
            }
        });
        // SAFETY: preserve the delegated allocation and reallocation contract.
        unsafe { System.realloc(pointer, layout, size) }
    }
}
pub fn measure<T>(run: impl FnOnce() -> T) -> (T, usize, usize) {
    ALLOCATIONS.with(|counter| counter.set(Some(0)));
    ALLOCATED_BYTES.with(|counter| counter.set(Some(0)));
    let output = run();
    let count = ALLOCATIONS.with(|counter| counter.replace(None).unwrap());
    let bytes = ALLOCATED_BYTES.with(|counter| counter.replace(None).unwrap());
    (output, count, bytes)
}
