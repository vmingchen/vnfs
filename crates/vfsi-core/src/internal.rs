//! Unstable implementation contracts shared by VFSI backend crates.
//!
//! Nothing in this module is part of the application-facing API. Backends
//! use these types to retain ordered per-request results until a strict
//! vector API converts them into `Result<Vec<_>, _>`.

use crate::{VfError, VfResult};

/// Ordered results produced by a backend `*_many` implementation.
///
/// Entry `n` always corresponds to request `n`. The vector may be shorter
/// than `requested` when an ordered protocol stopped after a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManyResults<T> {
    requested: usize,
    results: Vec<Result<T, VfError>>,
}

impl<T> ManyResults<T> {
    /// Construct ordered backend results.
    ///
    /// Backends may report a contiguous prefix. Invalid cardinality is kept
    /// so strict collection can report a structured contract error instead
    /// of panicking inside an application process.
    pub fn new(requested: usize, results: Vec<Result<T, VfError>>) -> Self {
        Self { requested, results }
    }

    pub fn all_success(values: Vec<T>) -> Self {
        let requested = values.len();
        Self::new(requested, values.into_iter().map(Ok).collect())
    }

    pub fn requested_len(&self) -> usize {
        self.requested
    }

    pub fn reported_len(&self) -> usize {
        self.results.len()
    }

    pub fn is_complete(&self) -> bool {
        self.results.len() == self.requested
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Result<T, VfError>> {
        self.results.iter()
    }

    pub fn into_results(self) -> Vec<Result<T, VfError>> {
        self.results
    }

    /// Apply the strict public policy: return every value or the first error.
    ///
    /// The vector position is authoritative and is attached to the returned
    /// error, regardless of any backend-local index it previously carried.
    pub fn try_collect(self) -> VfResult<Vec<T>> {
        let requested = self.requested;
        let reported = self.results.len();
        if reported > requested {
            return Err(result_count_error(reported, requested));
        }
        let mut values = Vec::with_capacity(requested);
        for (index, result) in self.results.into_iter().enumerate() {
            values.push(result.map_err(|error| error.with_index(index))?);
        }
        if values.len() != requested {
            return Err(result_count_error(values.len(), requested));
        }
        Ok(values)
    }

    /// Apply the strict public policy while cleaning every successful value
    /// if any request failed or the backend returned a short result set.
    ///
    /// Cleanup is best-effort and deliberately cannot replace the primary
    /// operation error. This is used by handle-producing vector operations,
    /// for which simply dropping `T` may not release a remote resource.
    pub fn try_collect_with_cleanup(
        self,
        expected: usize,
        mut cleanup: impl FnMut(usize, &T) -> VfResult<()>,
    ) -> VfResult<Vec<T>> {
        let declared = self.requested;
        let reported = self.results.len();
        let mut values = Vec::with_capacity(expected);
        let mut first_error = None;

        for (index, result) in self.results.into_iter().enumerate() {
            match result {
                Ok(value) => values.push((index, value)),
                Err(error) if first_error.is_none() => {
                    first_error = Some(error.with_index(index));
                }
                Err(_) => {}
            }
        }

        let primary = (declared != expected)
            .then(|| declared_count_error(declared, expected))
            .or_else(|| (reported > expected).then(|| result_count_error(reported, expected)))
            .or(first_error)
            .or_else(|| (reported != expected).then(|| result_count_error(reported, expected)));

        if let Some(error) = primary {
            for (index, value) in &values {
                let _ = cleanup(*index, value);
            }
            return Err(error);
        }
        Ok(values.into_iter().map(|(_, value)| value).collect())
    }
}

fn declared_count_error(declared: usize, expected: usize) -> VfError {
    VfError::transport(
        None,
        format!("backend declared {declared} requests for {expected} inputs"),
    )
}

fn result_count_error(reported: usize, requested: usize) -> VfError {
    VfError::transport(
        None,
        format!("backend reported {reported} results for {requested} requests"),
    )
}

impl<T> IntoIterator for ManyResults<T> {
    type Item = Result<T, VfError>;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.results.into_iter()
    }
}

/// Deterministic fault injection used by backend contract and integration
/// tests. This module is absent from ordinary production builds.
#[cfg(feature = "test-faults")]
pub mod faults {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use crate::{VfError, VfResult};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum OpenFaultPoint {
        BeforeDispatch { chunk: usize },
        AfterReply { chunk: usize },
        BeforeRegister { index: usize },
        AfterRegister { index: usize },
        BeforeCleanup { index: usize },
        BeforeCloseDispatch { index: usize },
        AfterWriteChunk { chunk: usize },
        BeforeSetPermissions { index: usize },
        BeforeRemoveType { index: usize },
        BeforeOpenChunk { chunk: usize },
        BeforeCloseItem { index: usize },
        BeforePathClose,
        BeforePathCloseBatch,
    }

    pub trait FaultInjector: Send + Sync {
        fn check(&self, point: &OpenFaultPoint) -> VfResult<()>;
    }

    /// An ordered, one-shot fault script. A configured fault fires only when
    /// its exact point is reached; tests can assert that no script entries
    /// remain afterward.
    #[derive(Debug)]
    pub struct FaultScript {
        remaining: Mutex<VecDeque<(OpenFaultPoint, VfError)>>,
        visited: Mutex<Vec<OpenFaultPoint>>,
    }

    impl FaultScript {
        pub fn one(point: OpenFaultPoint, error: VfError) -> Self {
            Self::new([(point, error)])
        }

        pub fn new(script: impl IntoIterator<Item = (OpenFaultPoint, VfError)>) -> Self {
            Self {
                remaining: Mutex::new(script.into_iter().collect()),
                visited: Mutex::new(Vec::new()),
            }
        }

        pub fn is_consumed(&self) -> bool {
            self.remaining
                .lock()
                .expect("fault script poisoned")
                .is_empty()
        }

        pub fn remaining(&self) -> Vec<OpenFaultPoint> {
            self.remaining
                .lock()
                .expect("fault script poisoned")
                .iter()
                .map(|(point, _)| point.clone())
                .collect()
        }

        pub fn visited(&self) -> Vec<OpenFaultPoint> {
            self.visited.lock().expect("fault script poisoned").clone()
        }
    }

    impl FaultInjector for FaultScript {
        fn check(&self, point: &OpenFaultPoint) -> VfResult<()> {
            self.visited
                .lock()
                .expect("fault script poisoned")
                .push(point.clone());
            let mut remaining = self.remaining.lock().expect("fault script poisoned");
            if remaining
                .front()
                .is_some_and(|(expected, _)| expected == point)
            {
                return Err(remaining.pop_front().expect("front exists").1);
            }
            Ok(())
        }
    }
}
