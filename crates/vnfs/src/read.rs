//! Private read dispatch projections and their regression tests.
#[cfg(test)]
use crate::{Error, OwnedReadResult, ReadOp};
pub(crate) use vfsi_core::api::internal::{ReadRequest, consume_ops, read_batch};
#[cfg(test)]
mod tests {
    use super::*;
    use crate::VfsiExt;
    #[cfg(all(feature = "auto", target_os = "linux"))]
    #[test]
    fn consuming_dispatch_rejects_bad_reply_shapes_and_does_not_replay() {
        for count in [0, 2] {
            let error = consume_ops::<crate::MountedFile>(
                [ReadOp::whole("/a")],
                16,
                |_, _| {
                    Ok((0..count)
                        .map(|_| OwnedReadResult {
                            offset: 0,
                            data: vec![],
                            eof: true,
                        })
                        .collect())
                },
                |_, _| panic!("no buffer operations"),
            )
            .unwrap_err();
            assert!(error.is_transport());
        }
        let temp = tempfile::tempdir().unwrap();
        let fs = crate::Mounted::new(temp.path()).unwrap();
        fs.write("/a", b"abc").unwrap();
        let file = fs.open("/a").unwrap();
        for (count, read) in [(0, 1), (2, 1), (1, 17)] {
            let mut buffer = [0; 1];
            let error = consume_ops(
                [ReadOp::into(&file, 0, &mut buffer)],
                16,
                |_, _| panic!("no owned operations"),
                |_, _| {
                    Ok(vec![
                        crate::ReadIntoResult {
                            offset: 0,
                            read,
                            eof: false
                        };
                        count
                    ])
                },
            )
            .unwrap_err();
            assert!(error.is_transport());
        }
        let mut calls = 0;
        let mut buffer = [0; 1];
        let error = consume_ops(
            [ReadOp::whole("/a"), ReadOp::into(&file, 0, &mut buffer)],
            16,
            |_, _| panic!("failure must stop later phases"),
            |_, _| {
                calls += 1;
                Err(Error::transport(None, "lost reply"))
            },
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(error.index(), None);
        buffer.fill(7);
    }
    #[test]
    fn malformed_cardinality_and_budget_are_errors() {
        for count in [0, 2] {
            let error = read_batch(
                &[ReadRequest::range(())],
                1,
                |_, _| {
                    Ok((0..count)
                        .map(|_| OwnedReadResult {
                            offset: 0,
                            data: vec![0],
                            eof: false,
                        })
                        .collect())
                },
                |_, _| panic!("no paths"),
            )
            .unwrap_err();
            assert!(error.is_transport());
        }
        for count in [0, 2] {
            let error = read_batch::<()>(
                &[ReadRequest::whole_file("/a")],
                1,
                |_, _| panic!("no ranges"),
                |_, _| Ok(vec![vec![]; count]),
            )
            .unwrap_err();
            assert!(error.is_transport());
        }
        let error = read_batch::<()>(
            &[ReadRequest::whole_file("/a")],
            1,
            |_, _| panic!("no ranges"),
            |_, _| Ok(vec![vec![0, 1]]),
        )
        .unwrap_err();
        assert!(error.is_transport());
    }
    #[test]
    fn transport_errors_are_not_attributed_or_replayed() {
        let mut calls = 0;
        let error = read_batch::<()>(
            &[ReadRequest::whole_file("/a")],
            1,
            |_, _| panic!("no ranges"),
            |_, _| {
                calls += 1;
                Err(Error::transport(None, "lost reply"))
            },
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(error.index(), None);
        let error = read_batch::<()>(
            &[ReadRequest::whole_file("/a")],
            1,
            |_, _| panic!("no ranges"),
            |_, _| Err(Error::transport(Some(9), "bad index")),
        )
        .unwrap_err();
        assert_eq!(error.index(), None);
        assert!(
            error
                .to_string()
                .contains("invalid vread_native backend error index")
        );
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
