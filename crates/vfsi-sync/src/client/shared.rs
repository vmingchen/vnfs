//! Connection locking, deferred cleanup, and final-owner teardown.

use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

fn record_cleanup_error(
    diagnostics: &Mutex<(std::collections::VecDeque<VfError>, u64)>,
    error: VfError,
) {
    let mut diagnostics = diagnostics.lock().unwrap_or_else(|e| e.into_inner());
    if diagnostics.0.len() == 32 {
        diagnostics.0.pop_front();
        diagnostics.1 = diagnostics.1.saturating_add(1);
    }
    diagnostics.0.push_back(error);
}

pub(super) enum CleanupTarget {
    File(FileOperand, PathBuf),
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
    pub(super) cursor_owner: Arc<()>,
    pending: Mutex<Vec<PendingClose<F>>>,
    has_pending: AtomicBool,
    diagnostics: Mutex<(std::collections::VecDeque<VfError>, u64)>,
}
impl<F: HandleBackend> SharedBackend<F> {
    pub(super) fn new(backend: F) -> Self {
        Self {
            backend: Mutex::new(Some(backend)),
            cursor_owner: Arc::new(()),
            pending: Mutex::new(Vec::new()),
            has_pending: AtomicBool::new(false),
            diagnostics: Mutex::new(Default::default()),
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
                diagnostics: &self.diagnostics,
            })
            .map_err(|_| ())
    }
    pub(super) fn take_cleanup_errors(&self) -> (Vec<VfError>, u64) {
        let mut diagnostics = self.diagnostics.lock().unwrap_or_else(|e| e.into_inner());
        let dropped = std::mem::take(&mut diagnostics.1);
        (diagnostics.0.drain(..).collect(), dropped)
    }
    pub(super) fn lock(&self) -> Result<BackendGuard<'_, F>, ()> {
        let mut guard = self.lock_without_cleanup()?;
        // Ordinary I/O must not lose its result to an unrelated cleanup error.
        // Retain failures; drain_cleanup is the explicit reporting boundary.
        let _ = guard.drain_cleanup();
        Ok(guard)
    }

    /// Consume the uniquely owned backend after draining cleanup once more.
    /// A handle can enqueue its deferred close after an earlier shared drain
    /// and before the last `Arc` is released, so exclusive extraction is the
    /// final point at which those requests can still be reported and retried.
    pub(super) fn into_backend(mut self) -> Result<F, Self> {
        let (error, callbacks) = {
            let backend = self
                .backend
                .get_mut()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_mut()
                .expect("live backend");
            let pending = self
                .pending
                .get_mut()
                .unwrap_or_else(|poison| poison.into_inner());
            let mut failed = Vec::new();
            let mut first = None;
            for close in std::mem::take(pending) {
                if let Err(error) = (close.close)(backend, &close.target) {
                    record_cleanup_error(&self.diagnostics, error.clone());
                    first.get_or_insert(error);
                    failed.push(close);
                }
            }
            pending.extend(failed);
            self.has_pending
                .store(!pending.is_empty(), Ordering::Release);
            (first, backend.take_notifications())
        };

        for callback in callbacks {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback));
        }
        if error.is_some() {
            return Err(self);
        }
        Ok(self
            .backend
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
            .expect("owned backend"))
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
    diagnostics: &'a Mutex<(std::collections::VecDeque<VfError>, u64)>,
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
        let outcomes = self.drain_cleanup_outcomes();
        let count = outcomes.len();
        vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
    }

    pub(super) fn drain_cleanup_outcomes(&mut self) -> Vec<vfsi_core::OpResult<()>> {
        if !self.has_pending.swap(false, Ordering::AcqRel) {
            return Vec::new();
        }
        let pending = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        let mut failed = Vec::new();
        let mut outcomes = Vec::with_capacity(pending.len());
        for close in pending {
            match (close.close)(self, &close.target) {
                Ok(()) => outcomes.push(Ok(())),
                Err(error) => {
                    // This bounded diagnostic history never owns cleanup targets.
                    // Keep ownership in pending, and preserve the operation's primary error.
                    record_cleanup_error(self.diagnostics, error.clone());
                    outcomes.push(Err(vfsi_core::OpError::unknown(error, None)));
                    failed.push(close);
                }
            }
        }
        if !failed.is_empty() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(failed);
            self.has_pending.store(true, Ordering::Release);
        }
        outcomes
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
