use super::*;
use std::{
    io::{self, Write},
    num::NonZeroUsize,
};

fn options(chunk: usize, budget: usize) -> WriterCopyOptions {
    WriterCopyOptions::from(StreamOptions::new().with_chunk_size(NonZeroUsize::new(chunk).unwrap()))
        .with_round_robin(NonZeroUsize::new(budget).unwrap())
}

fn source_handles(root: &Path) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|path| path.starts_with(root) && path != root)
        .count()
}

struct TraceWriter<'a> {
    fs: &'a Harness,
    index: usize,
    bytes: Vec<u8>,
    events: &'a RefCell<Vec<(usize, Vec<u8>)>>,
    fail_close: bool,
}
impl Write for TraceWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.fs.capabilities().unwrap(); // Writer code may reenter the client.
        if self.fail_close {
            self.fs.fault.set(Fault::Close);
        }
        self.events.borrow_mut().push((self.index, data.to_vec()));
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        panic!("copy helper must not flush caller-owned writers")
    }
}

#[test]
fn default_order_is_unchanged_and_round_robin_is_explicit() {
    let (root, fs) = fixture();
    fs.fs.write("/a", b"abcdef").unwrap();
    fs.fs.write("/b", b"12345").unwrap();
    let stream = StreamOptions::new().with_chunk_size(NonZeroUsize::new(3).unwrap());
    for round in [false, true] {
        let events = RefCell::new(Vec::new());
        let mut writers = [
            TraceWriter {
                fs: &fs,
                index: 0,
                bytes: b"prefix".to_vec(),
                events: &events,
                fail_close: false,
            },
            TraceWriter {
                fs: &fs,
                index: 1,
                bytes: Vec::new(),
                events: &events,
                fail_close: false,
            },
        ];
        let policy = if round {
            WriterCopyOptions::from(stream).with_round_robin(NonZeroUsize::new(6).unwrap())
        } else {
            WriterCopyOptions::from(stream)
        };
        assert_eq!(
            copy_to_writers(&fs, &["/a", "/b"], &mut writers, policy).unwrap(),
            [6, 5]
        );
        assert_eq!(writers[0].bytes, b"prefixabcdef");
        assert_eq!(writers[1].bytes, b"12345");
        let indices: Vec<_> = events.borrow().iter().map(|event| event.0).collect();
        assert_eq!(
            indices,
            if round {
                vec![0, 1, 0, 1]
            } else {
                vec![0, 0, 1, 1]
            }
        );
        assert_eq!(source_handles(root.path()), 0);
    }
    assert_eq!(WriterCopyOptions::new(), WriterCopyOptions::default());
    assert_eq!(
        WriterCopyOptions::new(),
        WriterCopyOptions::from(StreamOptions::new())
    );
}

#[test]
fn round_robin_short_reads_and_heterogeneous_files_have_no_gaps() {
    let (root, fs) = fixture();
    fs.fault.set(Fault::OwnedShortRead);
    let paths = ["/src/empty", "/src/a", "/src/nested/b"];
    let mut writers = [Vec::new(), Vec::new(), Vec::new()];
    assert_eq!(
        copy_to_writers(&fs, &paths, &mut writers, options(8, 24)).unwrap(),
        [0, 151, 6]
    );
    assert!(writers[0].is_empty());
    assert_eq!(writers[1], payload());
    assert_eq!(writers[2], b"second");
    assert_eq!(fs.calls.borrow().closes, 3);
    assert_eq!(source_handles(root.path()), 0);
}

#[test]
fn round_robin_bounds_payload_and_open_handles_and_shrinks_chunks() {
    for (chunk, budget, capacity) in [(3, 9, 3), (1, usize::MAX, 32), (9, 2, 1)] {
        let root = tempfile::tempdir().unwrap();
        let fs = Harness::new(root.path());
        let paths: Vec<_> = (0..40).map(|i| format!("/f{i}")).collect();
        for (i, path) in paths.iter().enumerate() {
            fs.fs.write(path, &[i as u8]).unwrap();
        }
        let mut writers = vec![Vec::new(); paths.len()];
        assert_eq!(
            copy_to_writers(&fs, &paths, &mut writers, options(chunk, budget)).unwrap(),
            vec![1; 40]
        );
        for (i, data) in writers.iter().enumerate() {
            assert_eq!(data, &[i as u8]);
        }
        let calls = fs.calls.borrow();
        assert_eq!(calls.open_batches[0], capacity);
        assert!(calls.open_batches.iter().all(|&count| count <= capacity));
        for batch in &calls.read_batches {
            assert!(batch.len() <= capacity);
            assert!(batch.iter().map(|r| r.1).sum::<usize>() <= budget.min(64));
            assert!(batch.iter().all(|r| r.1 <= chunk));
        }
        assert_eq!(calls.closes, 40);
        assert_eq!(source_handles(root.path()), 0);
    }
}

#[test]
fn round_robin_validates_shape_and_zero_client_budget_before_io() {
    let root = tempfile::tempdir().unwrap();
    let mut fs = Harness::new(root.path());
    fs.fs = fs
        .fs
        .with_limits(ResourceLimits::new().with_max_read_bytes(0));
    let mut empty: [Vec<u8>; 0] = [];
    assert!(
        copy_to_writers(&fs, &[] as &[&str], &mut empty, options(1, 1))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        copy_to_writers(&fs, &["/missing"], &mut empty, options(1, 1))
            .unwrap_err()
            .raw_code(),
        libc::EINVAL as u32
    );
    let mut writer = [Vec::new()];
    assert_eq!(
        copy_to_writers(&fs, &["/missing"], &mut writer, options(1, 1))
            .unwrap_err()
            .raw_code(),
        libc::EINVAL as u32
    );
    assert_eq!(fs.calls.borrow().opens, 0);
}

struct Limited<'a> {
    fs: &'a Harness,
    bytes: Vec<u8>,
    remaining: usize,
    errno: i32,
    calls: usize,
}
impl Write for Limited<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.calls += 1;
        if self.remaining == 0 {
            self.fs.fault.set(Fault::Close); // Cleanup must not hide the writer failure.
            return Err(io::Error::from_raw_os_error(self.errno));
        }
        let count = data.len().min(self.remaining).min(2);
        self.bytes.extend_from_slice(&data[..count]);
        self.remaining -= count;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        panic!("must not flush")
    }
}

#[test]
fn round_robin_partial_write_and_cancellation_stop_without_replay() {
    for errno in [libc::ENOSPC, libc::ECANCELED] {
        let (root, fs) = fixture();
        let mut writers = [99, 1, 99].map(|remaining| Limited {
            fs: &fs,
            bytes: Vec::new(),
            remaining,
            errno,
            calls: 0,
        });
        let error = copy_to_writers(
            &fs,
            &["/src/a", "/src/nested/b", "/src/a"],
            &mut writers,
            options(3, 9),
        )
        .unwrap_err();
        assert_eq!(error.raw_code(), errno as u32);
        assert_eq!(error.index(), Some(1));
        assert_eq!(error.operation(), Some("copy_to_writers"));
        assert_eq!(error.path(), Some(Path::new("/src/nested/b")));
        assert!(std::error::Error::source(&error).is_some());
        assert_eq!(writers[0].bytes, payload()[..3]);
        assert_eq!(writers[1].bytes, b"s");
        assert_eq!(writers[1].calls, 2);
        assert!(writers[2].bytes.is_empty());
        assert_eq!(writers[2].calls, 0);
        assert_eq!(fs.calls.borrow().read_attempts, 1);
        assert_eq!(fs.calls.borrow().closes, 3);
        assert_eq!(source_handles(root.path()), 0);
    }
}

#[test]
fn round_robin_maps_read_errors_after_finished_siblings_and_later_cohorts() {
    // The empty first stream finishes in round one; read index 1 in round two
    // belongs to original input 2, not original input 1.
    let (root, fs) = fixture();
    fs.fault.set(Fault::ReadAfter(2, 1));
    let mut writers = [Vec::new(), Vec::new(), Vec::new()];
    let error = copy_to_writers(
        &fs,
        &["/src/empty", "/src/a", "/src/nested/b"],
        &mut writers,
        options(4, 12),
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(2));
    assert_eq!(
        writers,
        [Vec::new(), payload()[..4].to_vec(), b"seco".to_vec()]
    );
    assert_eq!(fs.calls.borrow().read_attempts, 2);
    assert_eq!(fs.calls.borrow().closes, 3);
    assert_eq!(source_handles(root.path()), 0);

    let (root, fs) = fixture();
    fs.fault.set(Fault::ReadAfterOpen(3, 1));
    let paths = ["/src/a"; 8];
    let mut writers = vec![Vec::new(); paths.len()];
    let error = copy_to_writers(&fs, &paths, &mut writers, options(4, 8)).unwrap_err();
    assert_eq!(error.index(), Some(5));
    assert!(writers[..4].iter().all(|data| data == &payload()));
    assert!(writers[4..].iter().all(Vec::is_empty));
    assert_eq!(fs.calls.borrow().opens, 3);
    assert_eq!(fs.calls.borrow().closes, 6);
    assert_eq!(source_handles(root.path()), 0);
}

#[test]
fn round_robin_read_failures_and_bad_replies_close_without_retry() {
    for fault in [
        Fault::LostRead,
        Fault::BadRead,
        Fault::WrongOffset,
        Fault::ShortRead,
        Fault::NoReadProgress,
        Fault::OversizedOwnedRead,
        Fault::BadReadIndex,
    ] {
        let (root, fs) = fixture();
        fs.fault.set(fault);
        let mut writers = [Vec::new(), Vec::new()];
        let error = copy_to_writers(
            &fs,
            &["/src/a", "/src/nested/b"],
            &mut writers,
            options(4, 8),
        )
        .unwrap_err();
        if fault == Fault::LostRead {
            assert!(error.is_transport());
            assert_eq!(error.index(), None);
            assert!(error.to_string().contains("lost read reply"));
        } else {
            assert_eq!(error.transport_kind(), Some(TransportKind::InvalidReply));
        }
        assert!(writers.iter().all(Vec::is_empty));
        assert_eq!(fs.calls.borrow().read_attempts, 1);
        assert_eq!(fs.calls.borrow().closes, 2);
        assert_eq!(source_handles(root.path()), 0);
    }
}

#[test]
fn round_robin_open_failures_and_bad_open_counts_do_not_write() {
    for fault in [Fault::None, Fault::BadOpenCount, Fault::BadOpenIndex] {
        let (root, fs) = fixture();
        fs.fault.set(fault);
        let paths = if fault == Fault::None {
            ["/src/a", "/missing"]
        } else {
            ["/src/a", "/src/nested/b"]
        };
        let mut writers = [Vec::new(), Vec::new()];
        let error = copy_to_writers(&fs, &paths, &mut writers, options(4, 8)).unwrap_err();
        if fault == Fault::None {
            assert_eq!(error.raw_code(), libc::ENOENT as u32);
            assert_eq!(error.index(), Some(1));
        } else {
            assert_eq!(error.transport_kind(), Some(TransportKind::InvalidReply));
        }
        assert!(writers.iter().all(Vec::is_empty));
        assert_eq!(fs.calls.borrow().read_attempts, 0);
        assert_eq!(source_handles(root.path()), 0);
    }
}

#[test]
fn round_robin_close_errors_are_reported_and_malformed_indices_rejected() {
    for fault in [Fault::Close, Fault::BadCloseIndex] {
        let (root, fs) = fixture();
        fs.fault.set(fault);
        let mut writers = [Vec::new(), Vec::new()];
        let error = copy_to_writers(
            &fs,
            &["/src/a", "/src/nested/b"],
            &mut writers,
            options(4, 8),
        )
        .unwrap_err();
        assert_eq!(writers, [payload(), b"second".to_vec()]);
        if fault == Fault::Close {
            assert_eq!(error.raw_code(), libc::EIO as u32);
            assert_eq!(error.index(), Some(0));
        } else {
            assert_eq!(error.transport_kind(), Some(TransportKind::InvalidReply));
            assert_eq!(error.index(), None);
        }
        assert_eq!(fs.calls.borrow().closes, 2);
        assert_eq!(source_handles(root.path()), 0);
    }
}

#[test]
fn round_robin_empty_sources_and_eof_probes_never_call_writers() {
    #[derive(Default)]
    struct NonemptyWrites {
        bytes: Vec<u8>,
        calls: usize,
    }
    impl Write for NonemptyWrites {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.write_all(data)?;
            Ok(data.len())
        }
        fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
            assert!(!data.is_empty(), "empty read is not a writer callback");
            self.calls += 1;
            self.bytes.extend_from_slice(data);
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("must not flush")
        }
    }
    let (root, fs) = fixture();
    let mut writers = [NonemptyWrites::default(), NonemptyWrites::default()];
    assert_eq!(
        copy_to_writers(
            &fs,
            &["/src/empty", "/src/nested/b"],
            &mut writers,
            options(3, 6)
        )
        .unwrap(),
        [0, 6],
    );
    assert_eq!(writers[0].calls, 0);
    assert_eq!(writers[1].calls, 2);
    assert_eq!(writers[1].bytes, b"second");
    assert_eq!(source_handles(root.path()), 0);
}

#[test]
fn round_robin_later_close_error_retains_global_index_and_partial_effects() {
    let (root, fs) = fixture();
    let events = RefCell::new(Vec::new());
    let mut writers: Vec<_> = (0..6)
        .map(|index| TraceWriter {
            fs: &fs,
            index,
            bytes: Vec::new(),
            events: &events,
            fail_close: index == 2,
        })
        .collect();
    let error = copy_to_writers(&fs, &["/src/a"; 6], &mut writers, options(4, 8)).unwrap_err();
    assert_eq!(error.index(), Some(2));
    assert_eq!(error.raw_code(), libc::EIO as u32);
    assert!(writers[..4].iter().all(|w| w.bytes == payload()));
    assert!(writers[4..].iter().all(|w| w.bytes.is_empty()));
    assert_eq!(fs.calls.borrow().opens, 2);
    assert_eq!(fs.calls.borrow().closes, 4);
    assert_eq!(source_handles(root.path()), 0);
}
