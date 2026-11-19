#![cfg(all(feature = "posix", unix))]
use vnfs::files::{ReadOp, VfsiExt};
use vnfs::posix::Posix;

// Run this target with only `posix`: neither Auto nor NFS may supply the facade.
#[test]
fn standalone_posix_facade_is_rooted_and_rejects_missing_roots() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a"), b"alpha").unwrap();
    let fs = Posix::new(root.path()).unwrap();
    let results = fs
        .read_batch([ReadOp::whole("/a")], Default::default())
        .unwrap();
    assert_eq!(results[0].data(), Some(b"alpha".as_slice()));
    assert_eq!(fs.read("/a").unwrap(), b"alpha");
    let missing = root.path().join("missing");
    assert!(Posix::new(&missing).is_err());
    assert!(!missing.exists());
}

#[test]
#[cfg(target_pointer_width = "64")]
fn owned_range_allocation_failure_returns_an_error() {
    use std::io::ErrorKind;
    use std::num::NonZeroUsize;
    use vnfs::error::FailureState;
    use vnfs::files::{ReadOptions, Vfsi};

    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a"), b"alpha").unwrap();
    let fs = Posix::new(root.path()).unwrap();
    let file = fs.open("/a").unwrap();
    let length = isize::MAX as usize;
    let options = ReadOptions::new().with_max_total_bytes(NonZeroUsize::new(length + 1).unwrap());
    let error = fs
        .vread([ReadOp::range(&file, 0, length)], options)
        .unwrap()
        .remove(0)
        .unwrap_err();
    assert_eq!(error.state(), FailureState::NotAttempted);
    assert_eq!(error.cause().kind(), ErrorKind::OutOfMemory);

    let results = fs
        .vread(
            [ReadOp::range(&file, 0, length), ReadOp::range(&file, 1, 1)],
            options,
        )
        .unwrap();
    assert_eq!(
        results[0].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert_eq!(results[1].as_ref().unwrap().data(), Some(b"l".as_slice()));

    let mut buffer = [0; 1];
    let results = fs
        .vread(
            [
                ReadOp::range(&file, 0, length),
                ReadOp::into(&file, 0, &mut buffer),
            ],
            options,
        )
        .unwrap();
    let error = results[0].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::NotAttempted);
    assert_eq!(error.cause().kind(), ErrorKind::OutOfMemory);
    assert_eq!(results[1].as_ref().unwrap().bytes_read(), 1);
    assert_eq!(&buffer, b"a");
    file.close().unwrap();
}

#[test]
fn streams_and_writer_helpers_remain_ready_with_a_fifo() {
    use std::num::NonZeroUsize;
    use std::ops::ControlFlow;
    use vnfs::error::{StatusCode, native_status};
    use vnfs::files::{StreamCompletion, StreamOptions, Vfsi};
    use vnfs::helpers::{WriterCopyOptions, copy_to_writers};

    if vfsi_sync::test_support::supervise_with_deadline(
        "streams_and_writer_helpers_remain_ready_with_a_fifo",
    ) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let fifo =
        std::ffi::CString::new(root.path().join("pipe").as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: fifo names the pipe in this test's temporary directory.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    std::fs::write(root.path().join("file"), b"readable").unwrap();
    let fs = Posix::new(root.path()).unwrap();

    fn check_stream(fs: &impl Vfsi) {
        let mut data = Vec::new();
        let result = fs
            .vstream(
                &["/pipe", "/file"],
                StreamOptions::new(),
                |index, _, bytes| {
                    assert_eq!(index, 1);
                    data.extend_from_slice(bytes);
                    Ok(ControlFlow::Continue(()))
                },
            )
            .unwrap();
        assert_eq!(
            native_status(result[0].as_ref().unwrap_err().cause()),
            Some(StatusCode::Errno(libc::ESPIPE as u32))
        );
        assert_eq!(result[1].as_ref().unwrap(), &StreamCompletion::Complete);
        assert_eq!(data, b"readable");

        let mut writers = [Vec::new(), Vec::new()];
        let error = copy_to_writers(
            fs,
            &["/pipe", "/file"],
            &mut writers,
            WriterCopyOptions::new().with_round_robin(NonZeroUsize::new(64).unwrap()),
        )
        .unwrap_err();
        assert_eq!(
            native_status(error.cause()),
            Some(StatusCode::Errno(libc::ESPIPE as u32))
        );
        assert_eq!(error.index(), Some(0));

        let error = fs.write("/pipe", b"write").unwrap_err();
        assert_eq!(
            native_status(&error),
            Some(StatusCode::Errno(libc::ENXIO as u32))
        );
        let error = fs
            .write_files(&[("/pipe", b"x".as_slice()), ("/sink", b"y".as_slice())])
            .unwrap_err();
        assert_eq!(error.index(), Some(0));
        assert_eq!(
            native_status(error.cause()),
            Some(StatusCode::Errno(libc::ENXIO as u32))
        );
        fs.write("/sink", b"ready").unwrap();
        assert_eq!(fs.read("/sink").unwrap(), b"ready");
    }

    check_stream(&fs);
    #[cfg(all(feature = "auto", target_os = "linux"))]
    check_stream(&vnfs::mounted::Auto::new(root.path()).unwrap());
}
