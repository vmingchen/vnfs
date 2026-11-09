use crate::api::{StreamCompletion, StreamOptions, Vfsi, VfsiExt};
use std::{num::NonZeroUsize, ops::ControlFlow};
use vfsi_core::{FailureState, IoProgress, VfError};

/// Shared public contract; the caller owns and removes the fixture directory.
pub fn public_stream_results(fs: &impl Vfsi, directory: &str) {
    let options = StreamOptions::new().with_chunk_size(NonZeroUsize::new(2).unwrap());
    assert!(
        fs.vstream::<&str>(&[], options, |_, _, _| panic!("empty stream"))
            .unwrap()
            .is_empty()
    );
    if let Err(error) = fs.create_dir(directory) {
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    }
    let first = format!("{directory}/first");
    let last = format!("{directory}/last");
    let missing = format!("{directory}/missing");
    fs.write(&first, b"abcdef").unwrap();
    fs.write(&last, b"uvwxyz").unwrap();
    let paths = [&first, &missing, &last];
    let mut bytes = [Vec::new(), Vec::new(), Vec::new()];
    let outcomes = fs
        .vstream(&paths, options, |index, offset, data| {
            assert_eq!(offset, bytes[index].len() as u64);
            assert!(data.len() <= 2);
            // Callbacks must not hold the backend lock.
            fs.capabilities()?;
            bytes[index].extend_from_slice(data);
            Ok(ControlFlow::Continue(()))
        })
        .unwrap();
    assert_eq!(outcomes.len(), 3);
    assert_eq!(outcomes[0].as_ref().unwrap(), &StreamCompletion::Complete);
    assert_eq!(outcomes[2].as_ref().unwrap(), &StreamCompletion::Complete);
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::Failed);
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
    assert!(error.progress().is_none());
    assert_eq!(bytes, [b"abcdef".to_vec(), vec![], b"uvwxyz".to_vec()]);

    let mut calls = [0; 2];
    let outcomes = fs
        .vstream(&[&first, &last], options, |index, offset, _| {
            calls[index] += 1;
            if index == 0 && offset == 2 {
                Err(VfError::errno(17, libc::EACCES as u32))
            } else {
                Ok(ControlFlow::Continue(()))
            }
        })
        .unwrap();
    let error = outcomes[0].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::Failed);
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().raw_code(), libc::EACCES as u32);
    assert_eq!(
        error.progress(),
        Some(&IoProgress::Stream { next_offset: 2 })
    );
    assert_eq!(outcomes[1].as_ref().unwrap(), &StreamCompletion::Complete);
    assert_eq!(
        calls,
        [2, 3],
        "callback errors isolate files without replay"
    );

    let mut calls = 0;
    let outcomes = fs
        .vstream(&paths, options, |index, offset, _| {
            calls += 1;
            assert_eq!((index, offset), (0, 0));
            Ok(ControlFlow::Break(()))
        })
        .unwrap();
    assert_eq!(outcomes.len(), 3);
    assert_eq!(
        outcomes[0].as_ref().unwrap(),
        &StreamCompletion::Stopped { next_offset: 2 }
    );
    assert!(outcomes[1].is_err(), "stop retains the known failed OPEN");
    assert_eq!(
        outcomes[2].as_ref().unwrap(),
        &StreamCompletion::Stopped { next_offset: 0 }
    );
    assert_eq!(calls, 1);

    // More than one bounded cohort: an early failure cannot block later waves.
    let paths: Vec<_> = (0..40)
        .map(|index| if index == 1 { &missing } else { &last })
        .collect();
    let mut received = [0; 40];
    let outcomes = fs
        .vstream(&paths, options, |index, offset, data| {
            assert_eq!(offset, received[index]);
            received[index] += data.len() as u64;
            Ok(ControlFlow::Continue(()))
        })
        .unwrap();
    assert_eq!(outcomes.len(), 40);
    for (index, outcome) in outcomes.into_iter().enumerate() {
        if index == 1 {
            assert!(outcome.is_err());
            assert_eq!(received[index], 0);
        } else {
            assert_eq!(outcome.unwrap(), StreamCompletion::Complete);
            assert_eq!(received[index], 6);
        }
    }
}
