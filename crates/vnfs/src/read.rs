//! Private read dispatch projections and their regression tests.
#[cfg(test)]
use crate::{Error, ReadOp};
#[cfg(test)]
use vfsi_core::api::internal::OwnedReadResult;
pub(crate) use vfsi_core::api::internal::consume_ops;
#[cfg(any(test, all(feature = "auto", target_os = "linux")))]
pub(crate) use vfsi_core::api::internal::{ReadRequest, read_batch};
#[cfg(test)]
mod tests {
    use super::*;
    use crate::VfsiExt;
    use vfsi_core::{FailureState, OpError};
    fn failure<T>(mut outcomes: Vec<vfsi_core::OpResult<T>>) -> OpError {
        assert_eq!(outcomes.len(), 1);
        outcomes.remove(0).err().expect("unsuccessful input")
    }
    #[cfg(all(feature = "auto", target_os = "linux"))]
    #[test]
    fn consuming_dispatch_rejects_bad_reply_shapes_and_does_not_replay() {
        for count in [0, 2] {
            let error = failure(
                consume_ops::<crate::PosixFile, (), ()>(
                    [ReadOp::whole("/a")],
                    16,
                    |_, _, _| (),
                    |_, _, _| (),
                    |_, _| {
                        Ok((0..count)
                            .map(|_| {
                                Ok(OwnedReadResult {
                                    offset: 0,
                                    data: Vec::new(),
                                    eof: true,
                                })
                            })
                            .collect())
                    },
                    |_, _| panic!("no buffers"),
                )
                .unwrap(),
            );
            assert!(error.cause().is_transport());
            assert_eq!(error.state(), FailureState::Unknown);
        }
        let root = tempfile::tempdir().unwrap();
        let fs = crate::Posix::new(root.path()).unwrap();
        fs.write("/a", b"abc").unwrap();
        let file = fs.open("/a").unwrap();
        for (count, read) in [(0, 1), (2, 1), (1, 17)] {
            let mut buffer = [0; 1];
            let error = failure(
                consume_ops(
                    [ReadOp::into(&file, 0, &mut buffer)],
                    16,
                    |_, _, _| (),
                    |_, _, buffer| buffer,
                    |_, _| panic!("no owned reads"),
                    |_, _| {
                        Ok(vec![
                            Ok(crate::ReadIntoResult {
                                offset: 0,
                                bytes_read: read,
                                eof: false,
                            });
                            count
                        ])
                    },
                )
                .unwrap(),
            );
            assert!(error.cause().is_transport());
            assert_eq!(error.state(), FailureState::Unknown);
        }
        let mut calls = 0;
        let owned_calls = std::cell::Cell::new(0);
        let mut buffer = [99; 1];
        let outcomes = consume_ops(
            [ReadOp::whole("/a"), ReadOp::into(&file, 0, &mut buffer)],
            16,
            |_, _, _| (),
            |_, _, buffer| buffer,
            |_, _| {
                owned_calls.set(owned_calls.get() + 1);
                Ok(vec![Ok(OwnedReadResult {
                    offset: 0,
                    data: b"abc".to_vec(),
                    eof: true,
                })])
            },
            |_, _| {
                calls += 1;
                Err(Error::transport(None, "rejected before buffer dispatch"))
            },
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(owned_calls.get(), 1);
        assert_eq!(outcomes[0].as_ref().unwrap().data().unwrap(), b"abc");
        let error = outcomes[1].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::NotAttempted);
        assert_eq!(error.cause().index(), None);
        assert_eq!(buffer, [99]);
        buffer.fill(7);
    }

    #[test]
    fn malformed_cardinality_and_budget_are_errors() {
        for count in [0, 2] {
            let error = failure(
                read_batch(
                    &[ReadRequest::range(())],
                    1,
                    |_, _| {
                        Ok((0..count)
                            .map(|_| {
                                Ok(OwnedReadResult {
                                    offset: 0,
                                    data: vec![0],
                                    eof: false,
                                })
                            })
                            .collect())
                    },
                    |_, _| panic!("no paths"),
                )
                .unwrap(),
            );
            assert!(error.cause().is_transport());
            assert_eq!(error.state(), FailureState::Unknown);
            let error = failure(
                read_batch::<()>(
                    &[ReadRequest::whole_file("/a")],
                    1,
                    |_, _| panic!("no ranges"),
                    |_, _| Ok(vec![Ok(vec![]); count]),
                )
                .unwrap(),
            );
            assert!(error.cause().is_transport());
            assert_eq!(error.state(), FailureState::Unknown);
        }
        let error = failure(
            read_batch::<()>(
                &[ReadRequest::whole_file("/a")],
                1,
                |_, _| panic!("no ranges"),
                |_, _| Ok(vec![Ok(vec![0, 1])]),
            )
            .unwrap(),
        );
        assert!(error.cause().is_transport());
        assert_eq!(error.state(), FailureState::Unknown);
    }

    #[test]
    fn transport_errors_are_not_attributed_or_replayed() {
        let mut calls = 0;
        let error = failure(
            read_batch::<()>(
                &[ReadRequest::whole_file("/a")],
                1,
                |_, _| panic!("no ranges"),
                |_, _| {
                    calls += 1;
                    Err(Error::transport(
                        None,
                        "connection rejected before dispatch",
                    ))
                },
            )
            .unwrap(),
        );
        assert_eq!(calls, 1);
        assert_eq!(error.state(), FailureState::NotAttempted);
        assert_eq!(error.cause().index(), None);
        let error = failure(
            read_batch::<()>(
                &[ReadRequest::whole_file("/a")],
                1,
                |_, _| panic!("no ranges"),
                |_, _| Err(Error::transport(Some(9), "bad index")),
            )
            .unwrap(),
        );
        assert_eq!(error.cause().index(), None);
        assert!(
            error
                .cause()
                .to_string()
                .contains("invalid read backend error index")
        );
        let mut calls = 0;
        let error = failure(
            read_batch::<()>(
                &[ReadRequest::whole_file("/a")],
                1,
                |_, _| panic!("no ranges"),
                |_, _| {
                    calls += 1;
                    Ok(vec![Err(OpError::unknown(
                        Error::transport(None, "lost reply"),
                        None,
                    ))])
                },
            )
            .unwrap(),
        );
        assert_eq!(calls, 1);
        assert_eq!(error.state(), FailureState::Unknown);
        assert_eq!(error.cause().index(), None);
    }

    #[test]
    fn empty_requests_do_not_dispatch() {
        assert!(
            read_batch::<()>(
                &[],
                0,
                |_, _| panic!("range dispatch"),
                |_, _| panic!("path dispatch")
            )
            .unwrap()
            .is_empty()
        );
    }
}
