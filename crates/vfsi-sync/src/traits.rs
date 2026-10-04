use super::*;

/// Backend-owned continuation state for a paged directory visit.
///
/// The state is dropped automatically when the caller stops or an error
/// occurs, so backends do not need an explicit cursor-close operation.
#[doc(hidden)]
pub struct DirPageCursor(Box<dyn std::any::Any + Send>);

impl DirPageCursor {
    pub fn new<T: std::any::Any + Send>(state: T) -> Self {
        Self(Box::new(state))
    }

    pub fn is<T: std::any::Any + Send>(&self) -> bool {
        self.0.is::<T>()
    }

    pub fn into_state<T: std::any::Any + Send>(self) -> VfResult<T> {
        self.0
            .downcast::<T>()
            .map(|state| *state)
            .map_err(|_| VfError::client(0, ERR_INVAL))
    }
}

/// Backend page plus optional child seeds aligned with its entries.
/// A seed starts a child's first page relative to its observed parent.
#[doc(hidden)]
pub type BackendDirectoryPage = (
    Vec<VfAttrs>,
    Option<DirPageCursor>,
    Vec<Option<DirPageCursor>>,
);

/// Owned application page and traversal-scoped anchored child seeds.
#[doc(hidden)]
pub type DirectoryPage = (
    crate::DirectoryListing,
    Option<DirPageCursor>,
    Vec<(std::path::PathBuf, DirPageCursor)>,
);

/// Resource limits for reading multiple complete files into memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadAllOptions {
    max_total_bytes: usize,
}

impl ReadAllOptions {
    pub const fn new() -> Self {
        Self {
            max_total_bytes: DEFAULT_READ_ALLV_MAX_TOTAL_BYTES,
        }
    }

    /// Set the maximum combined size of all returned buffers.
    pub const fn max_total_bytes(mut self, bytes: usize) -> Self {
        self.max_total_bytes = bytes;
        self
    }

    pub const fn total_byte_limit(self) -> usize {
        self.max_total_bytes
    }
}

impl Default for ReadAllOptions {
    fn default() -> Self {
        Self::new()
    }
}

fn contract_error(
    operation: &str,
    index: Option<usize>,
    detail: impl std::fmt::Display,
) -> VfError {
    VfError::transport(
        index,
        format!("{operation} backend contract violation: {detail}"),
    )
}

pub(crate) fn take_single_result<T>(operation: &str, mut results: Vec<T>) -> VfResult<T> {
    if results.len() != 1 {
        return Err(contract_error(
            operation,
            None,
            format!("returned {} results for one request", results.len()),
        ));
    }
    Ok(results.pop().expect("validated one result"))
}

pub(crate) fn validate_read_results(
    operation: &str,
    requests: &[ReadOp],
    results: &[ReadResult],
) -> VfResult<()> {
    if results.len() != requests.len() {
        return Err(contract_error(
            operation,
            None,
            format!(
                "returned {} results for {} requests",
                results.len(),
                requests.len()
            ),
        ));
    }
    for (index, (request, result)) in requests.iter().zip(results).enumerate() {
        if result.file != request.file {
            return Err(contract_error(
                operation,
                Some(index),
                "result file does not match request",
            ));
        }
        if result.data.len() > request.length {
            return Err(contract_error(
                operation,
                Some(index),
                format!(
                    "returned {} bytes for a {}-byte read",
                    result.data.len(),
                    request.length
                ),
            ));
        }
        if let VfOffset::At(expected) = request.offset
            && result.offset != expected
        {
            return Err(contract_error(
                operation,
                Some(index),
                format!("result offset {} does not match {expected}", result.offset),
            ));
        }
        if request.length != 0 && result.data.is_empty() && !result.eof {
            return Err(contract_error(
                operation,
                Some(index),
                "read made no progress without reporting EOF",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_read_into_results(
    operation: &str,
    requests: &[ReadOp],
    results: &[ReadIntoResult],
) -> VfResult<()> {
    if results.len() != requests.len() {
        return Err(contract_error(
            operation,
            None,
            format!(
                "returned {} results for {} requests",
                results.len(),
                requests.len()
            ),
        ));
    }
    for (index, (request, result)) in requests.iter().zip(results).enumerate() {
        if result.file != request.file {
            return Err(contract_error(
                operation,
                Some(index),
                "result file does not match request",
            ));
        }
        if result.read > request.length {
            return Err(contract_error(
                operation,
                Some(index),
                format!(
                    "returned {} bytes for a {}-byte read",
                    result.read, request.length
                ),
            ));
        }
        if let VfOffset::At(expected) = request.offset
            && result.offset != expected
        {
            return Err(contract_error(
                operation,
                Some(index),
                format!("result offset {} does not match {expected}", result.offset),
            ));
        }
        if request.length != 0 && result.read == 0 && !result.eof {
            return Err(contract_error(
                operation,
                Some(index),
                "read made no progress without reporting EOF",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_write_results(
    operation: &str,
    requests: &[WriteOpRef<'_>],
    results: &[WriteResult],
) -> VfResult<()> {
    if results.len() != requests.len() {
        return Err(contract_error(
            operation,
            None,
            format!(
                "returned {} results for {} requests",
                results.len(),
                requests.len()
            ),
        ));
    }
    for (index, (request, result)) in requests.iter().zip(results).enumerate() {
        if result.file != *request.file {
            return Err(contract_error(
                operation,
                Some(index),
                "result file does not match request",
            ));
        }
        if result.written > request.data.len() {
            return Err(contract_error(
                operation,
                Some(index),
                format!(
                    "reported {} bytes written for a {}-byte write",
                    result.written,
                    request.data.len()
                ),
            ));
        }
        if let VfOffset::At(expected) = request.offset
            && result.offset != expected
        {
            return Err(contract_error(
                operation,
                Some(index),
                format!("result offset {} does not match {expected}", result.offset),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    fn assert_contract_error(error: VfError, index: Option<usize>, detail: &str) {
        assert_eq!(error.domain(), vfsi_core::ErrorDomain::Transport);
        assert_eq!(error.index(), index);
        assert_eq!(error.status(), None);
        let message = error.to_string();
        assert!(message.contains("backend contract violation"), "{message}");
        assert!(message.contains(detail), "{message}");
    }

    fn request() -> ReadOp {
        ReadOp::at(VfFile::from_path("/file"), 0, 1)
    }

    fn result() -> ReadResult {
        ReadResult {
            file: VfFile::from_path("/file"),
            offset: 0,
            data: vec![1],
            eof: true,
        }
    }

    #[test]
    fn read_result_cardinality_is_checked_before_indexing() {
        let requests = [request(), request()];
        validate_read_results("test", &requests, &[result(), result()]).unwrap();
        validate_read_results("test", &[], &[]).unwrap();
        assert_contract_error(
            validate_read_results("test", &requests, &[result()]).unwrap_err(),
            None,
            "returned 1 results for 2 requests",
        );
        assert_contract_error(
            validate_read_results("test", &requests[..1], &[result(), result()]).unwrap_err(),
            None,
            "returned 2 results for 1 requests",
        );
    }

    #[test]
    fn read_result_identity_offset_progress_and_size_are_checked() {
        let request = request();
        validate_read_results("test", std::slice::from_ref(&request), &[result()]).unwrap();
        let larger = ReadOp::at(VfFile::from_path("/file"), 0, 4);
        validate_read_results(
            "test",
            &[larger],
            &[ReadResult {
                eof: false,
                ..result()
            }],
        )
        .unwrap();
        validate_read_results(
            "test",
            std::slice::from_ref(&request),
            &[ReadResult {
                data: Vec::new(),
                eof: true,
                ..result()
            }],
        )
        .unwrap();
        for malformed in [
            ReadResult {
                file: VfFile::from_path("/other"),
                ..result()
            },
            ReadResult {
                offset: 1,
                ..result()
            },
            ReadResult {
                data: vec![1, 2],
                ..result()
            },
            ReadResult {
                data: Vec::new(),
                eof: false,
                ..result()
            },
        ] {
            // Keep a valid prefix so attribution must identify the second
            // request rather than always returning request zero.
            assert_contract_error(
                validate_read_results(
                    "test",
                    &[request.clone(), request.clone()],
                    &[result(), malformed],
                )
                .unwrap_err(),
                Some(1),
                "test",
            );
        }
    }

    #[test]
    fn write_result_identity_offset_size_and_cardinality_are_checked() {
        let file = VfFile::from_path("/file");
        let request = WriteOpRef::new(&file, VfOffset::At(0), b"x");
        let valid = WriteResult {
            file: file.clone(),
            offset: 0,
            written: 1,
            stable: true,
        };
        validate_write_results("test", &[request], std::slice::from_ref(&valid)).unwrap();
        // Partial/zero progress is a valid backend result; vwrite_all_native owns
        // the policy for completing it or rejecting a no-progress loop.
        validate_write_results(
            "test",
            &[request],
            &[WriteResult {
                written: 0,
                ..valid.clone()
            }],
        )
        .unwrap();
        validate_write_results("test", &[], &[]).unwrap();
        assert_contract_error(
            validate_write_results("test", &[request], &[]).unwrap_err(),
            None,
            "returned 0 results",
        );
        assert_contract_error(
            validate_write_results("test", &[request], &[valid.clone(), valid.clone()])
                .unwrap_err(),
            None,
            "returned 2 results",
        );
        for malformed in [
            WriteResult {
                file: VfFile::from_path("/other"),
                ..valid.clone()
            },
            WriteResult {
                offset: 1,
                ..valid.clone()
            },
            WriteResult {
                written: 2,
                ..valid.clone()
            },
        ] {
            assert_contract_error(
                validate_write_results("test", &[request, request], &[valid.clone(), malformed])
                    .unwrap_err(),
                Some(1),
                "test",
            );
        }
    }

    #[test]
    fn scalar_result_cardinality_is_checked_without_panicking() {
        assert_contract_error(
            take_single_result::<u8>("readlink", Vec::new()).unwrap_err(),
            None,
            "readlink",
        );
        assert_contract_error(
            take_single_result("readlink", vec![1u8, 2]).unwrap_err(),
            None,
            "readlink",
        );
        assert_eq!(take_single_result("readlink", vec![7u8]).unwrap(), 7);
    }
}

#[cfg(test)]
mod walk_depth_tests {
    use super::{DepthLimit, WalkOptions};
    #[test]
    fn depth_limit_is_one_byte_and_preserves_optional_inheritance() {
        const UNLIMITED: DepthLimit = DepthLimit::new(201);
        assert_eq!(std::mem::size_of::<DepthLimit>(), 1);
        assert_eq!(std::mem::size_of::<Option<DepthLimit>>(), 1);
        assert!(UNLIMITED.is_unlimited());
        for depth in 0..=200 {
            let limit = DepthLimit::new(depth);
            assert!(!limit.is_unlimited());
            assert_eq!(limit.get(), depth);
        }
        for depth in [201, 255, 256, 500, usize::MAX - 1, usize::MAX] {
            assert_eq!(DepthLimit::new(depth), DepthLimit::unlimited());
        }
        assert_ne!(None, Some(DepthLimit::new(0)));
        assert_ne!(None, Some(DepthLimit::unlimited()));
    }
    #[test]
    fn depth_limits_normalize_without_wrapping_and_keep_other_options() {
        const UNLIMITED: WalkOptions = WalkOptions::new().max_depth(201);
        assert_eq!(UNLIMITED.depth_limit(), usize::MAX);
        for depth in 0..=200 {
            let options = WalkOptions::new()
                .max_depth(depth)
                .max_entries(7)
                .max_path_bytes(11)
                .truncate_at_max_depth(true);
            assert_eq!(options.depth_limit(), depth);
            assert_eq!(options.entry_limit(), 7);
            assert_eq!(options.path_byte_limit(), 11);
            assert!(options.truncates_at_depth_limit());
        }
        for depth in [201, 254, 255, 256, 500, usize::MAX - 1, usize::MAX] {
            let options = WalkOptions::new().max_depth(depth);
            assert_eq!(options.depth_limit(), usize::MAX);
            assert_eq!(options.max_depth(0).depth_limit(), 0);
            assert_eq!(options.max_depth(200).depth_limit(), 200);
        }
        assert_eq!(WalkOptions::unlimited().depth_limit(), usize::MAX);
    }
}
