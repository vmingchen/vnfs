use super::Result;
use std::path::Path;

/// Owned application file. Borrowed requests preserve the file's lifetime;
/// clients validate connection ownership before dispatching a vector.
///
/// Clients validate ownership before I/O and report positional progress.
/// Standard-I/O adapters manage their own independent cursors. `ReadOp` borrows handles directly; constructing it does no I/O.
/// A backend must retain ownership of live descriptors through cleanup failures.
pub trait FileHandle {
    /// Diagnostic name captured at open; not current path or object identity.
    fn path(&self) -> &Path;
    /// Close without surrendering cleanup ownership on failure. After an
    /// ambiguous failure, reconcile/close rather than resume ordinary I/O.
    fn try_close(&mut self) -> Result<()>;
    /// Local close ownership only, not proof of remote liveness after failure.
    fn is_closed(&self) -> bool;
    /// Consuming close: on failure the handle is lost and Drop retries cleanup
    /// best-effort on a later operation or cleanup drain. Prefer `try_close` when
    /// cleanup failures need reconciliation.
    fn close(self) -> Result<()>
    where
        Self: Sized;
}

/// Owned directory retaining the opened object's identity after rename or path
/// replacement. Paths are diagnostic only; clients validate ownership and live
/// state before handle-rooted operations. Failed `try_close` retains cleanup ownership.
pub trait DirHandle {
    /// Diagnostic name captured at open, not current path or identity.
    fn path(&self) -> &Path;
    /// Close while retaining cleanup ownership on failure.
    fn try_close(&mut self) -> Result<()>;
    /// Local close ownership, not proof of remote liveness after failure.
    fn is_closed(&self) -> bool;
    /// Consuming close; on failure Drop queues best-effort cleanup.
    fn close(self) -> Result<()>
    where
        Self: Sized;
}
