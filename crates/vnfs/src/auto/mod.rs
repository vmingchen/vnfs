//! Linux mount-aware client. Only unambiguous NFSv4 AUTH_SYS mounts are
//! promoted to a separate direct NFS connection; everything else stays on
//! the kernel-mounted path.

use crate::Vfsi as _;
use crate::VfsiExt as _;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::{
    DirEntry, Error as VfError, Nfs, OpenFlags, OpenOp, OwnedReadResult as ReadResult, Posix,
    ReadIntoResult, ResourceLimits, Result as VfResult, WriteResult,
};
use vfsi_local::LocalBackend;
use vfsi_sync::{FsClient, FsFile};
type NfsClient = FsClient<vfsi_nfs::NfsVecFs>;
type NfsFile = FsFile<vfsi_nfs::NfsVecFs>;

impl Auto {
    /// Root all application paths at this existing host directory. For example,
    /// root `/work` plus application `/a` addresses host `/work/a`.
    /// The root is a namespace convention, not a security sandbox.
    pub fn new(root: impl AsRef<Path>) -> VfResult<Self> {
        let root = fs::canonicalize(root.as_ref())
            .map_err(|e| VfError::client(0, e.raw_os_error().unwrap_or(libc::EIO) as u32))?;
        if !root.is_dir() {
            return Err(VfError::client(0, libc::ENOTDIR as u32));
        }
        Ok(Self {
            mounted: Posix::new(&root)?.inner,
            root,
            connections: Mutex::new(HashMap::new()),
            owner: Arc::new(()),
            limits: ResourceLimits::default(),
        })
    }

    pub fn with_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self.mounted = self.mounted.with_limits(limits);
        // Keep the connection/route identities: already-open handles still
        // belong to these clients. Only replace their per-clone policies.
        for connection in self
            .connections
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .values_mut()
        {
            connection.client = connection.client.clone().with_limits(limits);
        }
        self
    }
}

/// The backend actually selected for a path. A direct route means a fresh
/// connection was successfully established and its root identity verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoRoute {
    Posix,
    DirectNfs {
        mount_point: PathBuf,
        server: String,
    },
}

struct RoutedDirectoryCursor {
    route: Route,
    backend_path: PathBuf,
    public_path: PathBuf,
    owner: Arc<()>,
    cursor: vfsi_sync::DirPageCursor,
}

struct RoutedChildDirectoryCursor(RoutedDirectoryCursor);

struct Resolved {
    route: Route,
    path: PathBuf,
}

/// Opt into mount-aware direct NFS acceleration. Unlike [`Posix`], eligible
/// NFSv4 AUTH_SYS mounts use a separate client and vectorized COMPOUNDs.
///
/// # Consistency
/// Direct operations do not share or invalidate the kernel client's caches.
/// Mixing them with kernel I/O on the same objects (including path aliases)
/// can expose stale data or delayed writes. This is not a transparent caching
/// acceleration. Use `Posix` when kernel coherency semantics are required.
pub struct Auto {
    root: PathBuf,
    mounted: FsClient<LocalBackend>,
    connections: Mutex<HashMap<u64, NfsConnection>>,
    owner: Arc<()>,
    limits: ResourceLimits,
}

impl std::fmt::Debug for Auto {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Auto")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

mod routing;
use routing::{MountTable, NfsConnection, Route, read_mounts};
use vfsi_nfs::mount::AuthSysIdentity;
mod handles;
pub use handles::{AutoDir, AutoFile};
use handles::{AutoDirInner, AutoFileInner, AutoRead, AutoReadInto};
mod dispatch;
#[cfg(test)]
mod tests;
