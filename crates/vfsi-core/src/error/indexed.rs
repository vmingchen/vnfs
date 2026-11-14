//! Input attribution retained by deliberately lossy multi-input helpers.
use std::{fmt, io, path::PathBuf};

/// A helper-selected cause, without operation certainty or confirmed progress.
/// Use VecResult to retain those details and independent sibling outcomes.
/// None is unattributed and does not imply that no work was dispatched.
#[derive(Debug)]
pub struct IndexedError {
    index: Option<usize>,
    cause: io::Error,
}
impl IndexedError {
    pub fn at(index: usize, cause: io::Error) -> Self {
        Self {
            index: Some(index),
            cause,
        }
    }
    pub fn unattributed(cause: io::Error) -> Self {
        Self { index: None, cause }
    }
    pub fn index(&self) -> Option<usize> {
        self.index
    }
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }
    pub fn into_cause(self) -> io::Error {
        self.cause
    }
    pub fn into_parts(self) -> (Option<usize>, io::Error) {
        (self.index, self.cause)
    }
    pub fn map_index(mut self, map: impl FnOnce(usize) -> usize) -> Self {
        self.index = self.index.map(map);
        self
    }
    pub fn with_context(mut self, operation: &'static str, path: impl Into<PathBuf>) -> Self {
        self.cause = super::standard::with_context(self.cause, operation, path);
        self
    }
}
impl fmt::Display for IndexedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(index) = self.index {
            write!(f, "input {index}: ")?;
        }
        self.cause.fmt(f)
    }
}
impl std::error::Error for IndexedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}
