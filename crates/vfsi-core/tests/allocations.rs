//! Allocation regressions for dispatch and ADB planning, excluding payload storage.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    path::Path,
};
use vfsi_core::api::internal::{OwnedReadResult, ReadRequest, consume_ops, read_batch};
use vfsi_core::api::*;

struct TrackingAllocator;
thread_local! {
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
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
        // SAFETY: preserve the delegated allocation and reallocation contract.
        unsafe { System.realloc(pointer, layout, size) }
    }
}
fn measured<T>(run: impl FnOnce() -> T) -> (T, usize) {
    ALLOCATIONS.with(|counter| counter.set(Some(0)));
    let output = run();
    let count = ALLOCATIONS.with(|counter| counter.replace(None).unwrap());
    (output, count)
}
struct File;

#[test]
fn path_split_borrows_both_components_without_allocating() {
    let path = b"/parent/\xff//leaf///";
    let ((parent, leaf), allocations) =
        measured(|| vfsi_core::path::split_path_bytes(path).unwrap());
    assert_eq!(allocations, 0);
    assert_eq!(parent, b"parent/\xff");
    assert_eq!(leaf, b"leaf");
    assert_eq!(parent.as_ptr(), path[1..].as_ptr());
    assert_eq!(leaf.as_ptr(), path[11..].as_ptr());
}

#[test]
fn adb_planning_uses_constant_storage_even_for_huge_valid_counts() {
    let pattern = vfsi_core::Adb::blocknum_only("/unused", 0, 0, usize::MAX, 0, 0);
    let (mut layout, allocations) =
        measured(|| vfsi_core::internal::adb_layout(&pattern, 0).unwrap());
    assert_eq!(allocations, 0, "layout must not allocate per-block storage");
    assert_eq!(layout.len(), usize::MAX);
    assert_eq!(layout.next(), Some((0, Some(0), None)));
    assert_eq!(
        layout.next_back(),
        Some(((usize::MAX - 1) as u64, Some(0), None))
    );
}

impl FileHandle for File {
    fn path(&self) -> &Path {
        Path::new("/file")
    }
    fn try_close(&mut self) -> Result<()> {
        Ok(())
    }
    fn is_closed(&self) -> bool {
        false
    }
    fn close(self) -> Result<()> {
        Ok(())
    }
}
#[test]
fn singleton_buffer_reads_allocate_only_backend_results_and_output() {
    let mut buffer = [0; 1];
    let (result, allocations) = measured(|| {
        consume_ops(
            [ReadOp::into(&File, 7, &mut buffer)],
            1,
            |_, _, _| (),
            |_, offset, buffer| (offset, buffer),
            |_, _| panic!("no owned request"),
            |requests, _| {
                requests[0].1[0] = 42;
                Ok(vec![ReadIntoResult {
                    offset: 7,
                    bytes_read: 1,
                    eof: false,
                }])
            },
        )
        .unwrap()
    });
    assert_eq!(result[0].bytes_read(), 1);
    assert_eq!(buffer, [42]);
    assert!(
        allocations <= 2,
        "singleton dispatcher allocated {allocations} times"
    );
}
#[test]
fn homogeneous_owned_reads_do_not_allocate_a_reordering_buffer() {
    let (result, allocations) = measured(|| {
        consume_ops::<File, (), ()>(
            [
                ReadOp::whole("/a"),
                ReadOp::whole("/b"),
                ReadOp::whole("/c"),
            ],
            1,
            |_, _, _| (),
            |_, _, _| (),
            |requests, _| {
                Ok(requests
                    .iter()
                    .map(|_| OwnedReadResult {
                        offset: 0,
                        data: Vec::new(),
                        eof: true,
                    })
                    .collect())
            },
            |_, _| panic!("no buffered request"),
        )
        .unwrap()
    });
    assert_eq!(result.len(), 3);
    assert!(
        allocations <= 3,
        "homogeneous dispatcher allocated {allocations} times"
    );
}

#[test]
fn homogeneous_dispatch_staging_stays_constant_across_batch_sizes() {
    for count in [1, 3, 32, 1024] {
        let (results, allocations) = measured(|| {
            consume_ops::<File, (), ()>(
                (0..count).map(|_| ReadOp::whole("/file")),
                0,
                |_, _, _| (),
                |_, _, _| (),
                |requests, budget| {
                    read_batch(
                        requests,
                        budget,
                        |_, _| panic!("no ranges"),
                        |paths, _| Ok(paths.iter().map(|_| Vec::new()).collect()),
                    )
                },
                |_, _| panic!("no buffers"),
            )
            .unwrap()
        });
        assert_eq!(results.len(), count);
        assert!(
            allocations <= 4,
            "{count} owned requests allocated {allocations} times"
        );

        let mut buffers = vec![[0; 1]; count];
        let (results, allocations) = measured(|| {
            consume_ops(
                buffers
                    .iter_mut()
                    .map(|buffer| ReadOp::into(&File, 7, buffer)),
                count,
                |_, _, _| (),
                |_, offset, buffer| (offset, buffer),
                |_, _| panic!("no owned reads"),
                |requests, _| {
                    Ok(requests
                        .iter_mut()
                        .map(|(offset, buffer)| {
                            buffer[0] = 42;
                            ReadIntoResult {
                                offset: *offset,
                                bytes_read: 1,
                                eof: false,
                            }
                        })
                        .collect())
                },
            )
            .unwrap()
        });
        assert_eq!(results.len(), count);
        assert!(buffers.iter().all(|buffer| buffer == &[42]));
        assert!(
            allocations <= 3,
            "{count} buffered requests allocated {allocations} times"
        );
    }
}

#[test]
fn mixed_dispatch_keeps_order_and_error_indices_with_unknown_iterator_lengths() {
    for count in [3, 32, 1024] {
        for buffered_first in [false, true] {
            for fail in [false, true] {
                let mut buffers = vec![[]; count];
                let ops = buffers
                    .iter_mut()
                    .enumerate()
                    .map(|(index, buffer)| {
                        if (index % 8 == 0) == buffered_first {
                            ReadOp::into(&File, index as u64, buffer)
                        } else {
                            ReadOp::range(&File, index as u64, 0)
                        }
                    })
                    .filter(|_| true); // intentionally no exact lower size hint
                let complete = |offset| -> vfsi_core::VfResult<_> {
                    if fail && offset as usize == count - 1 {
                        Err(vfsi_core::VfError::errno(0, libc::ENOENT as u32))
                    } else {
                        Ok(offset)
                    }
                };
                let results = consume_ops(
                    ops,
                    0,
                    |_, offset, _| offset,
                    |_, offset, _| offset,
                    |requests, _| {
                        requests
                            .iter()
                            .enumerate()
                            .map(|(index, request)| {
                                let offset = complete(*request.range_ref().unwrap())
                                    .map_err(|e| e.with_index(index))?;
                                Ok(OwnedReadResult {
                                    offset,
                                    data: Vec::new(),
                                    eof: true,
                                })
                            })
                            .collect()
                    },
                    |requests, _| {
                        requests
                            .iter()
                            .enumerate()
                            .map(|(index, offset)| {
                                let offset = complete(*offset).map_err(|e| e.with_index(index))?;
                                Ok(ReadIntoResult {
                                    offset,
                                    bytes_read: 0,
                                    eof: true,
                                })
                            })
                            .collect()
                    },
                );
                if fail {
                    assert_eq!(results.unwrap_err().index(), Some(count - 1));
                } else {
                    assert_eq!(
                        results
                            .unwrap()
                            .iter()
                            .map(ReadResult::offset)
                            .collect::<Vec<_>>(),
                        (0..count as u64).collect::<Vec<_>>()
                    );
                }
            }
        }
    }
}

#[test]
fn singleton_preflight_and_backend_indices_do_not_replay_or_mutate() {
    let mut buffer = [9; 1];
    let error = consume_ops(
        [ReadOp::into(&File, 0, &mut buffer)],
        0,
        |_, _, _| (),
        |_, _, _| (),
        |_, _| panic!("budget failure must precede dispatch"),
        |_, _| panic!("budget failure must precede dispatch"),
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(buffer, [9]);
    let error = consume_ops::<File, (), ()>(
        [ReadOp::whole("/a")],
        0,
        |_, _, _| (),
        |_, _, _| (),
        |_, _| Err(Error::errno(1, 5)),
        |_, _| panic!("no buffered request"),
    )
    .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), None);
}

#[test]
fn range_batches_reuse_backend_result_storage() {
    let requests = [ReadRequest::range(7), ReadRequest::range(11)];
    let replies: Vec<_> = [7, 11]
        .into_iter()
        .map(|offset| OwnedReadResult {
            offset,
            data: vec![offset as u8],
            eof: true,
        })
        .collect();
    let storage = replies.as_ptr();
    let payloads: Vec<_> = replies.iter().map(|reply| reply.data.as_ptr()).collect();
    let (result, allocations) = measured(|| {
        read_batch(
            &requests,
            2,
            |ranges, budget| {
                assert_eq!(ranges, [&7, &11]);
                assert_eq!(budget, 2);
                Ok(replies)
            },
            |_, _| panic!("range-only batches must not submit path reads"),
        )
        .unwrap()
    });
    assert_eq!(
        result.as_ptr(),
        storage,
        "range results should retain backend storage"
    );
    assert!(
        allocations <= 2,
        "range dispatcher allocated {allocations} times"
    );
    for ((reply, payload), offset) in result.iter().zip(payloads).zip([7, 11]) {
        assert_eq!(reply.offset, offset);
        assert_eq!(reply.data, [offset as u8]);
        assert_eq!(reply.data.as_ptr(), payload);
        assert!(reply.eof);
    }
}
