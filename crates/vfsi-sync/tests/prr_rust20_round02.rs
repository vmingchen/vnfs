//! Closed-handle read preflight retains original request attribution.
use std::io::SeekFrom;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use vfsi_sync::api::{ReadOp as AppReadOp, ReadOptions};
use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::{
    FileOperand, FsClient, OpenOp, ReadOp, ReadResult, SetAttrsOp, Target, VfAttrs, VfError,
    VfOffset, VfResult, Vfsi, VfsiExt, WriteOp, WriteResult,
};

#[derive(Default)]
struct ReadBackend {
    next_fd: i32,
    dispatches: Arc<AtomicUsize>,
}
impl HandleBackend for ReadBackend {
    fn open_impl(&mut self, _: &OpenOp) -> VfResult<FileOperand> {
        self.next_fd += 1;
        Ok(FileOperand::from_fd(self.next_fd))
    }
    fn close_impl(&mut self, _: &FileOperand) -> VfResult<()> {
        Ok(())
    }
    fn sync_data(&mut self, _: &FileOperand) -> VfResult<()> {
        Ok(())
    }
    fn read_impl(&mut self, request: &ReadOp) -> VfResult<ReadResult> {
        let VfOffset::At(offset) = request.offset else {
            panic!("positional read expected")
        };
        let data = b"abcdef";
        let start = usize::try_from(offset).unwrap().min(data.len());
        let end = start.saturating_add(request.length).min(data.len());
        Ok(ReadResult {
            file: request.file.clone(),
            offset,
            data: data[start..end].to_vec(),
            eof: end == data.len(),
        })
    }
    fn write_impl(&mut self, _: WriteOp<&FileOperand, &[u8]>) -> VfResult<WriteResult> {
        Err(VfError::unsupported(0))
    }
    fn seek_impl(&mut self, _: &FileOperand, _: SeekFrom) -> VfResult<u64> {
        Err(VfError::unsupported(0))
    }
    fn metadata_impl(
        &mut self,
        _: Target<'_, FileOperand>,
        _: vfsi_sync::api::AttrsOptions,
    ) -> VfResult<VfAttrs> {
        Err(VfError::unsupported(0))
    }
    fn set_attributes_impl(&mut self, _: &SetAttrsOp<Target<'_, FileOperand>>) -> VfResult<()> {
        Err(VfError::unsupported(0))
    }
}
impl VectorBackend for ReadBackend {
    fn vread_impl(&mut self, requests: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        requests
            .iter()
            .map(|request| self.read_impl(request))
            .collect()
    }
}
fn fixture() -> (FsClient<ReadBackend>, Arc<AtomicUsize>) {
    let backend = ReadBackend::default();
    let dispatches = Arc::clone(&backend.dispatches);
    (FsClient::new(backend), dispatches)
}
fn assert_closed(error: VfError, index: usize, operation: &str) {
    assert_eq!(error.err_no(), vfsi_sync::ERR_EBADF);
    assert_eq!(error.index(), Some(index));
    assert_eq!(error.path(), Some(Path::new("/closed")));
    assert_eq!(error.operation(), Some(operation));
}
#[test]
fn owned_read_preflight_reports_closed_slot_including_zero_length() {
    for length in [0, 1] {
        for closed_index in 0..3 {
            let (client, dispatches) = fixture();
            let valid = client.open("/valid").unwrap();
            let mut closed = client.open("/closed").unwrap();
            closed.try_close().unwrap();
            let requests: Vec<_> = (0..3)
                .map(|index| {
                    AppReadOp::range(
                        if index == closed_index {
                            &closed
                        } else {
                            &valid
                        },
                        0,
                        length,
                    )
                })
                .collect();
            let error = client.vread(requests, ReadOptions::new()).unwrap_err();
            assert_closed(error, closed_index, "vread_native");
            assert_eq!(dispatches.load(Ordering::SeqCst), 0);
        }
    }
}
#[test]
fn buffer_read_preflight_reports_closed_slot_without_mutating_buffers() {
    for length in [0, 1] {
        for closed_index in 0..3 {
            let (client, dispatches) = fixture();
            let valid = client.open("/valid").unwrap();
            let mut closed = client.open("/closed").unwrap();
            closed.try_close().unwrap();
            let mut storage = [[0xcc; 1]; 3];
            let requests: Vec<_> = storage
                .iter_mut()
                .enumerate()
                .map(|(index, buffer)| {
                    AppReadOp::into(
                        if index == closed_index {
                            &closed
                        } else {
                            &valid
                        },
                        0,
                        &mut buffer[..length],
                    )
                })
                .collect();
            let error = client.vread(requests, ReadOptions::new()).unwrap_err();
            assert_closed(error, closed_index, "vread_into_native");
            assert_eq!(storage, [[0xcc; 1]; 3]);
            assert_eq!(dispatches.load(Ordering::SeqCst), 0);
        }
    }
}
#[test]
fn mixed_whole_and_range_preflight_remaps_closed_handle_to_original_slot() {
    let (client, dispatches) = fixture();
    let valid = client.open("/valid").unwrap();
    let mut closed = client.open("/closed").unwrap();
    closed.try_close().unwrap();
    let error = client
        .vread(
            [
                AppReadOp::whole("/whole"),
                AppReadOp::range(&valid, 0, 1),
                AppReadOp::whole("/other"),
                AppReadOp::range(&closed, 0, 1),
            ],
            ReadOptions::new(),
        )
        .unwrap_err();
    assert_closed(error, 3, "vread_native");
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
}
#[test]
fn mixed_buffer_preflight_remaps_closed_handle_before_any_dispatch() {
    let (client, dispatches) = fixture();
    let valid = client.open("/valid").unwrap();
    let mut closed = client.open("/closed").unwrap();
    closed.try_close().unwrap();
    let mut first = [0xcc];
    let mut second = [0xdd];
    let error = client
        .vread(
            [
                AppReadOp::range(&valid, 0, 1),
                AppReadOp::into(&valid, 0, &mut first),
                AppReadOp::whole("/whole"),
                AppReadOp::into(&closed, 0, &mut second),
            ],
            ReadOptions::new(),
        )
        .unwrap_err();
    assert_closed(error, 3, "vread_into_native");
    assert_eq!(first, [0xcc]);
    assert_eq!(second, [0xdd]);
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
}
#[test]
fn successful_range_and_buffer_batches_keep_one_dispatch_each() {
    let (client, dispatches) = fixture();
    let valid = client.open("/valid").unwrap();
    let results = client
        .vread(
            [
                AppReadOp::range(&valid, 0, 2),
                AppReadOp::range(&valid, 2, 2),
            ],
            ReadOptions::new(),
        )
        .unwrap();
    assert_eq!(results[0].data(), Some(b"ab".as_slice()));
    assert_eq!(results[1].data(), Some(b"cd".as_slice()));
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    let mut first = [0xcc; 2];
    let mut second = [0xdd; 2];
    let results = client
        .vread(
            [
                AppReadOp::into(&valid, 0, &mut first),
                AppReadOp::into(&valid, 4, &mut second),
            ],
            ReadOptions::new(),
        )
        .unwrap();
    assert_eq!(first, *b"ab");
    assert_eq!(second, *b"ef");
    assert!(
        results
            .iter()
            .all(|result| result.is_buffered() && result.read() == 2)
    );
    assert_eq!(dispatches.load(Ordering::SeqCst), 2);
}

#[test]
fn singleton_closed_read_controls_keep_index_zero() {
    let (client, dispatches) = fixture();
    let mut closed = client.open("/closed").unwrap();
    closed.try_close().unwrap();
    assert_closed(
        client
            .vread([AppReadOp::range(&closed, 0, 1)], ReadOptions::new())
            .unwrap_err(),
        0,
        "vread_native",
    );
    let mut buffer = [0xcc];
    assert_closed(
        client
            .vread(
                [AppReadOp::into(&closed, 0, &mut buffer)],
                ReadOptions::new(),
            )
            .unwrap_err(),
        0,
        "vread_into_native",
    );
    assert_eq!(buffer, [0xcc]);
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert_eq!(client.read("/valid").unwrap(), b"abcdef");
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
}
