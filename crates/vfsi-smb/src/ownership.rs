//! Session-scoped ownership for handles whose CLOSE must not be replayed.
use super::*;
use smb2::types::SessionId;
use vfsi_core::OpResult;

/// Shared once per connected share, rather than copying the SDK Tree's strings
/// into each retained file or error owner.
#[derive(Clone)]
pub(super) struct ShareSession {
    tree: Tree,
    connection: Connection,
    epoch: (u64, SessionId),
    #[cfg(test)]
    pub(super) close_submissions: Arc<std::sync::atomic::AtomicUsize>,
}

impl std::fmt::Debug for ShareSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShareSession")
            .field("tree_id", &self.tree.tree_id)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl std::ops::Deref for ShareSession {
    type Target = Tree;
    fn deref(&self) -> &Tree {
        &self.tree
    }
}

impl ShareSession {
    pub(super) fn new(tree: Tree, connection: Connection) -> Self {
        let epoch = (connection.generation(), connection.session_id());
        Self {
            tree,
            connection,
            epoch,
            #[cfg(test)]
            close_submissions: Arc::default(),
        }
    }

    pub(super) fn sdk_tree_mut(&mut self) -> &mut Tree {
        &mut self.tree
    }

    pub(super) fn refresh_connection(&mut self, connection: Connection) {
        self.epoch = (connection.generation(), connection.session_id());
        self.connection = connection;
    }

    pub(super) fn same_tree(&self, other: &Self) -> bool {
        self.epoch == other.epoch && self.tree.tree_id == other.tree.tree_id
    }

    /// SDK generation is the session identity. Compare the retained tree/epoch
    /// with this backend's live tree before sending any descriptor I/O. The
    /// generation load is atomic; do not lock SDK crypto once per input merely
    /// to reread the session ID already captured in each tree's epoch.
    pub(super) fn owns_live_file(&self, owner: &Self) -> bool {
        self.same_tree(owner) && self.epoch.0 == self.connection.generation()
    }

    pub(super) fn is_current(&self) -> bool {
        self.epoch == (self.connection.generation(), self.connection.session_id())
    }

    pub(super) async fn close_file(&self, file_id: FileId) -> OpResult<()> {
        if !self.is_current() {
            // Non-durable opens belong to the ended session. Never put their
            // FileId on a replacement session, where it may name another open.
            return Ok(());
        }
        #[cfg(test)]
        self.close_submissions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut connection = self.connection.clone();
        match self.tree.close_handle(&mut connection, file_id).await {
            Ok(()) => Ok(()),
            Err(_) if !self.is_current() => Ok(()),
            Err(error) => Err(protocol::wire_failure(smb_error(error, None), None)),
        }
    }

    pub(super) async fn disconnect(&self) -> VfRes {
        if !self.is_current() {
            return Ok(());
        }
        let mut connection = self.connection.clone();
        self.tree
            .disconnect(&mut connection)
            .await
            .map_err(|error| smb_error(error, None))
    }
}

/// Protocol cleanup evidence, independent of the share that owns the handle.
/// NoReplay retains ownership until share teardown without resubmitting CLOSE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloseState {
    Ready(FileId),
    NoReplay(FileId),
}
impl CloseState {
    pub(super) fn file_id(self) -> FileId {
        match self {
            Self::Ready(file_id) | Self::NoReplay(file_id) => file_id,
        }
    }
}

/// Exactly one backend-owned cleanup target, pinned to its original share.
pub(super) struct DeferredClose {
    pub(super) tree: Arc<ShareSession>,
    pub(super) state: CloseState,
}
impl DeferredClose {
    pub(super) fn new(tree: Arc<ShareSession>, state: CloseState) -> Self {
        Self { tree, state }
    }

    pub(super) fn can_attempt(&self) -> bool {
        matches!(self.state, CloseState::Ready(_))
    }

    pub(super) async fn attempt(&mut self) -> OpResult<()> {
        if !self.can_attempt() {
            return Err(vfsi_core::OpError::unknown(
                VfError::transport(None, "SMB CLOSE retained for session teardown"),
                None,
            ));
        }
        let id = self.state.file_id();
        self.state = CloseState::NoReplay(id);
        self.tree.close_file(id).await
    }
}
