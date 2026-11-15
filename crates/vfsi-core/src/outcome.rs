//! Independent vector outcomes. Result position identifies the input.

/// Rejection before dispatch, or one outcome for every accepted input.
/// After dispatch, infrastructure failures belong in the affected input slots.
pub type VecResult<T> = Result<Vec<OpResult<T>>, std::io::Error>;
pub type OpResult<T> = Result<T, OpError>;

/// Knowledge about an unsuccessful logical operation, not retry permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureState {
    /// No part of this logical input was submitted.
    NotAttempted,
    /// A definite unsuccessful outcome; confirmed partial effects are possible.
    Failed,
    /// Additional effects may have occurred beyond any confirmed progress.
    Unknown,
}

/// Confirmed I/O progress before a logical operation failed.
/// A read prefix is not a successful whole-file read; EOF does not indicate
/// whether the logical operation completed. Counts never describe sibling inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoProgress {
    Read(crate::api::ReadOutput),
    /// Contiguous acknowledged prefix of this input's write payload.
    /// Unknown completion can still mean additional bytes were written.
    Write {
        bytes_written: usize,
    },
    /// End of the contiguous prefix accepted by successful stream callbacks.
    /// The failing callback may have consumed additional bytes; this offset
    /// does not authorize replay or describe un-emitted prefetched data.
    Stream {
        next_offset: u64,
    },
}

/// One input's failure, with native cause and completion knowledge.
/// The vector slot supplies identity; the cause has no logical input index.
/// Progress storage is allocated only for errors with confirmed I/O progress.
#[derive(Debug)]
pub struct OpError {
    cause: std::io::Error,
    state: FailureState,
    progress: Option<Box<IoProgress>>,
}
impl OpError {
    pub fn not_attempted(cause: std::io::Error) -> Self {
        Self::new(cause, FailureState::NotAttempted, None)
    }
    pub fn failed(cause: std::io::Error, progress: Option<IoProgress>) -> Self {
        Self::new(cause, FailureState::Failed, progress)
    }
    pub fn unknown(cause: std::io::Error, progress: Option<IoProgress>) -> Self {
        Self::new(cause, FailureState::Unknown, progress)
    }
    fn new(cause: std::io::Error, state: FailureState, progress: Option<IoProgress>) -> Self {
        Self {
            cause,
            state,
            progress: progress.map(Box::new),
        }
    }
    /// Attach diagnostics without changing completion knowledge or progress.
    pub fn with_context(
        mut self,
        operation: &'static str,
        path: impl Into<std::path::PathBuf>,
    ) -> Self {
        self.cause = crate::internal::errors::with_context(self.cause, operation, path);
        self
    }
    pub fn cause(&self) -> &std::io::Error {
        &self.cause
    }
    pub const fn state(&self) -> FailureState {
        self.state
    }
    pub fn progress(&self) -> Option<&IoProgress> {
        self.progress.as_deref()
    }
    /// Move the cause, completion state and confirmed progress into an adapter
    /// without cloning an owned read prefix.
    pub fn into_parts(self) -> (std::io::Error, FailureState, Option<IoProgress>) {
        (
            self.cause,
            self.state,
            self.progress.map(|progress| *progress),
        )
    }
    /// Deliberately discard completion/progress information for a lossy adapter.
    /// This conversion provides no execution-order or retry guarantee.
    pub fn into_cause(self) -> std::io::Error {
        self.cause
    }
}
impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.state, self.cause)
    }
}
impl std::error::Error for OpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Explicitly retain a shared original without moving or copying confirmed I/O
/// progress. Kept in backend support, outside the application error API.
pub fn share_op_cause(error: &mut OpError) -> std::sync::Arc<std::io::Error> {
    crate::error::standard::share_in_place(&mut error.cause)
}
