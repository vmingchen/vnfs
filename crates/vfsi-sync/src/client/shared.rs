//! Connection locking, deferred cleanup, and final-owner teardown.

use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) enum CleanupTarget {
    File(VfFile, PathBuf),
    Directory(VfDir, PathBuf),
}
struct PendingClose<F> {
    target: CleanupTarget,
    close: fn(&mut F, &CleanupTarget) -> VfResult<()>,
}

/// The cleanup queue has a separate short-held lock: handle Drop never waits
/// for an RPC or for the backend mutex. Final-owner teardown remains synchronous.
pub(super) struct SharedBackend<F: HandleBackend> {
    pub(super) backend: Mutex<Option<F>>,
    pending: Mutex<Vec<PendingClose<F>>>,
    has_pending: AtomicBool,
}
impl<F: HandleBackend> SharedBackend<F> {
    pub(super) fn new(backend: F) -> Self {
        Self {
            backend: Mutex::new(Some(backend)),
            pending: Mutex::new(Vec::new()),
            has_pending: AtomicBool::new(false),
        }
    }
    pub(super) fn defer(
        &self,
        target: CleanupTarget,
        close: fn(&mut F, &CleanupTarget) -> VfResult<()>,
    ) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(PendingClose { target, close });
        self.has_pending.store(true, Ordering::Release);
    }
    pub(super) fn lock_without_cleanup(&self) -> Result<BackendGuard<'_, F>, ()> {
        self.backend
            .lock()
            .map(|guard| BackendGuard {
                guard: Some(guard),
                pending: &self.pending,
                has_pending: &self.has_pending,
            })
            .map_err(|_| ())
    }
    pub(super) fn lock(&self) -> Result<BackendGuard<'_, F>, ()> {
        let mut guard = self.lock_without_cleanup()?;
        // Ordinary I/O must not lose its result to an unrelated cleanup error.
        // Retain failures; drain_cleanup is the explicit reporting boundary.
        let _ = guard.drain_cleanup();
        Ok(guard)
    }
}
impl<F: HandleBackend> Drop for SharedBackend<F> {
    fn drop(&mut self) {
        if let Some(backend) = self
            .backend
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            let pending = self.pending.get_mut().unwrap_or_else(|e| e.into_inner());
            for close in pending.drain(..) {
                let _ = (close.close)(backend, &close.target);
            }
            let callbacks = backend.take_notifications();
            for callback in callbacks {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback));
            }
        }
    }
}
pub(super) struct BackendGuard<'a, F: HandleBackend> {
    guard: Option<std::sync::MutexGuard<'a, Option<F>>>,
    pending: &'a Mutex<Vec<PendingClose<F>>>,
    has_pending: &'a AtomicBool,
}
impl<F: HandleBackend> std::ops::Deref for BackendGuard<'_, F> {
    type Target = F;
    fn deref(&self) -> &F {
        self.guard
            .as_ref()
            .expect("live guard")
            .as_ref()
            .expect("live backend")
    }
}
impl<F: HandleBackend> std::ops::DerefMut for BackendGuard<'_, F> {
    fn deref_mut(&mut self) -> &mut F {
        self.guard
            .as_mut()
            .expect("live guard")
            .as_mut()
            .expect("live backend")
    }
}
impl<F: HandleBackend> BackendGuard<'_, F> {
    pub(super) fn drain_cleanup(&mut self) -> VfResult<()> {
        if !self.has_pending.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let pending = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        let mut failed = Vec::new();
        let mut first = None;
        for close in pending {
            if let Err(error) = (close.close)(self, &close.target) {
                first.get_or_insert(error);
                failed.push(close);
            }
        }
        if !failed.is_empty() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(failed);
            self.has_pending.store(true, Ordering::Release);
        }
        first.map_or(Ok(()), Err)
    }
}
impl<F: HandleBackend> Drop for BackendGuard<'_, F> {
    fn drop(&mut self) {
        let callbacks = self.take_notifications();
        drop(self.guard.take());
        for callback in callbacks {
            // Observability must not turn successful I/O into a panic or lose
            // ownership of a newly opened descriptor before its RAII wrapping.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback));
        }
    }
}
