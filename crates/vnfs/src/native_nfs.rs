//! Application-facing NFS constructor and concrete client aliases.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::{NfsClient, ResourceLimits, Result as VfResult};
use vfsi_nfs::{
    NfsAuthentication, NfsClientBuilder, NfsObserver, NfsReadPool, NfsReadPoolOptions,
    NfsRecoveryPolicy,
};
use vfsi_sync::FsClient;

/// Supported NFS protocol selection. Auto negotiates v4.2 then v4.1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NfsVersion {
    #[default]
    Auto,
    V4_1,
    V4_2,
}

impl TryFrom<Option<u32>> for NfsVersion {
    type Error = crate::Error;
    fn try_from(value: Option<u32>) -> crate::Result<Self> {
        match value {
            None => Ok(Self::Auto),
            Some(1) => Ok(Self::V4_1),
            Some(2) => Ok(Self::V4_2),
            _ => Err(crate::Error::client(0, libc::EINVAL as u32)),
        }
    }
}

/// Independent NFS sessions for caller-distributed parallel workloads.
/// Cloning a single [`NfsClient`] shares one lock; pool members do not.
#[derive(Clone, Debug)]
pub struct NfsClientPool {
    inner: Arc<NfsClientPoolInner>,
}

#[derive(Debug)]
struct NfsClientPoolInner {
    clients: Vec<NfsClient>,
    next: AtomicUsize,
}

impl NfsClientPool {
    pub fn len(&self) -> usize {
        self.inner.clients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.clients.is_empty()
    }

    /// Return a specific member; cloned handles share that member's session.
    pub fn client(&self, index: usize) -> Option<NfsClient> {
        self.inner.clients.get(index).cloned()
    }

    /// Choose the next independent session in round-robin order.
    pub fn next_client(&self) -> NfsClient {
        let index = self.inner.next.fetch_add(1, Ordering::Relaxed) % self.len();
        self.inner.clients[index].clone()
    }
}

/// Entry point for the Rust-native NFS API.
///
/// Use [`Nfs::builder`] to configure a direct connection, or
/// [`Nfs::from_mount`] to discover an existing Linux NFS-mounted directory.
/// See [`crate::examples`] for complete application workflows.
#[derive(Debug, Clone, Copy, Default)]
pub struct Nfs;

impl Nfs {
    /// Inspect a supported Linux NFS mount without connecting. Applications
    /// can group operands and translate paths without parsing /proc themselves.
    #[cfg(target_os = "linux")]
    pub fn discover_mount(path: impl AsRef<Path>) -> VfResult<NfsMount> {
        Ok(NfsMount {
            inner: vfsi_nfs::mount::NfsMount::discover(path)?,
        })
    }
    /// Connect directly using the configuration of a Linux NFS-mounted directory.
    /// Operations use the remote directory as their root, without sharing the
    /// kernel client's cache or falling back to mounted filesystem operations.
    /// For mount path `/mnt/nfs/project`, application `/a` names `project/a`
    /// remotely, not host `/a`. Do not mix direct and kernel access assuming
    /// cache coherence. This namespace root is not a security sandbox.
    pub fn from_mount(path: impl AsRef<Path>) -> VfResult<NfsClient> {
        NfsBuilder::from_mount(path)?.connect()
    }

    pub fn builder(host: impl Into<String>) -> NfsBuilder {
        NfsBuilder {
            inner: NfsClientBuilder::new(host),
            limits: ResourceLimits::default(),
        }
    }

    pub fn connect(host: impl Into<String>) -> VfResult<NfsClient> {
        Self::builder(host).connect()
    }
}

/// Opaque discovered mount configuration; no backend extraction or credentials.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct NfsMount {
    inner: vfsi_nfs::mount::NfsMount,
}
#[cfg(target_os = "linux")]
impl NfsMount {
    pub fn host(&self) -> &str {
        self.inner.host()
    }
    pub fn mount_point(&self) -> &Path {
        self.inner.mount_point()
    }
    pub fn export_root(&self) -> &Path {
        self.inner.export_root()
    }
    pub fn local_path(&self) -> &Path {
        self.inner.local_path()
    }
}

/// NFS configuration builder whose `connect` returns an owned native client.
#[derive(Debug, Clone)]
pub struct NfsBuilder {
    inner: NfsClientBuilder,
    limits: ResourceLimits,
}

impl NfsBuilder {
    /// Discover mount configuration, then customize timeouts and other tuning.
    pub fn from_mount(path: impl AsRef<Path>) -> VfResult<Self> {
        #[cfg(target_os = "linux")]
        {
            Ok(Self {
                inner: NfsClientBuilder::from_mount(path)?,
                limits: ResourceLimits::default(),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(crate::Error::client(0, libc::EOPNOTSUPP as u32)
                .with_context("from_mount requires Linux", path.as_ref()))
        }
    }

    /// Select an NFS-visible namespace directory, not a host-local directory.
    /// With root `/export/project`, application `/a` addresses `/export/project/a`
    /// remotely. Leading `/` in application paths means this configured root,
    /// not the server's namespace root. Rooting is not security confinement.
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.inner = self.inner.root(root);
        self
    }

    pub fn version(mut self, version: NfsVersion) -> Self {
        self.inner = self.inner.minor_version(match version {
            NfsVersion::Auto => None,
            NfsVersion::V4_1 => Some(1),
            NfsVersion::V4_2 => Some(2),
        });
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.inner = self.inner.connect_timeout(timeout);
        self
    }

    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.inner = self.inner.request_timeout(timeout);
        self
    }

    /// Select RPC authentication explicitly. Default AUTH_SYS conveys Unix
    /// identity but provides no cryptographic peer authentication or encryption.
    /// An explicitly requested secure mode must succeed or fail; no downgrade.
    pub fn auth(mut self, auth: NfsAuthentication) -> Self {
        self.inner = self.inner.authentication(auth);
        self
    }

    pub fn client_owner(mut self, owner: impl Into<Vec<u8>>) -> Self {
        self.inner = self.inner.client_owner(owner);
        self
    }

    pub fn recovery_policy(mut self, policy: NfsRecoveryPolicy) -> Self {
        self.inner = self.inner.recovery_policy(policy);
        self
    }

    pub fn auto_reconnect(mut self, enabled: bool) -> Self {
        self.inner = self.inner.auto_reconnect(enabled);
        self
    }

    pub fn max_compound_bytes(mut self, bytes: usize) -> Self {
        self.inner = self.inner.max_compound_bytes(bytes);
        self
    }

    pub fn observer(mut self, observer: Arc<dyn NfsObserver>) -> Self {
        self.inner = self.inner.observer(observer);
        self
    }

    pub fn connect(self) -> VfResult<NfsClient> {
        self.inner.connect().map(|backend| NfsClient {
            inner: FsClient::new(backend).with_limits(self.limits),
        })
    }

    /// Defaults for connected clients. Read pools instead use their supplied
    /// `NfsReadPoolOptions`, which bound concurrency and outstanding buffers.
    pub fn limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Connect 1–64 independent clients. Distribute separate vector cohorts
    /// across members; one vector call itself remains on one session.
    pub fn connect_pool(self, size: usize) -> VfResult<NfsClientPool> {
        let clients = self
            .inner
            .connect_pool(size)?
            .into_iter()
            .map(|backend| NfsClient {
                inner: FsClient::new(backend).with_limits(self.limits),
            })
            .collect();
        Ok(NfsClientPool {
            inner: Arc::new(NfsClientPoolInner {
                clients,
                next: AtomicUsize::new(0),
            }),
        })
    }

    /// Connect a reusable bounded pool for ordered pipelined large-file reads.
    pub fn connect_read_pool(self, options: NfsReadPoolOptions) -> VfResult<NfsReadPool> {
        self.inner.connect_read_pool(options)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod mount_tests {
    use super::*;

    #[test]
    #[ignore = "requires configured NFS mount fixtures"]
    fn live_mount_constructor_roots_vector_io_at_a_subdirectory() {
        let mount = std::env::var("VFSI_NFS_TEST_MOUNT")
            .expect("VFSI_NFS_TEST_MOUNT is required for this ignored integration test");
        let directory = Path::new(&mount).join(format!(".vnfs-from-mount-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let fs = NfsBuilder::from_mount(&directory)
            .unwrap()
            .request_timeout(Duration::from_secs(2))
            .connect()
            .unwrap();
        fs.write_files(&[
            ("/file-1", b"hello".as_slice()),
            ("/file-2", b"world".as_slice()),
        ])
        .unwrap();
        assert_eq!(
            fs.readv_with_options(
                ["/file-1", "/file-2"]
                    .iter()
                    .map(crate::ReadOp::whole)
                    .collect::<Vec<_>>(),
                crate::ReadOptions::default()
            )
            .unwrap()
            .into_iter()
            .map(|r| r.data.unwrap())
            .collect::<Vec<_>>(),
            [b"hello".to_vec(), b"world".to_vec()]
        );
        fs.remove_file("/file-1").unwrap();
        fs.remove_file("/file-2").unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    #[ignore = "requires configured NFS mount fixtures"]
    fn live_read_only_mount_rejects_mutations_before_dispatch() {
        let mount = std::env::var("VFSI_NFS_TEST_MOUNT_RO")
            .expect("VFSI_NFS_TEST_MOUNT_RO is required for this ignored integration test");
        let rw_mount = std::env::var("VFSI_NFS_TEST_MOUNT")
            .expect("read-only test needs a writable fixture mount");
        let name = format!(".vnfs-read-only-{}", std::process::id());
        let fixture = Path::new(&rw_mount).join(&name);
        std::fs::create_dir(&fixture).unwrap();
        std::fs::write(fixture.join("marker"), b"unchanged").unwrap();
        std::fs::create_dir(fixture.join("child")).unwrap();
        let fs = Nfs::from_mount(Path::new(&mount).join(&name)).unwrap();
        assert!(fs.metadata("/").unwrap().is_dir());
        let results = [
            fs.create("/.mount-forbidden").map(|_| ()),
            fs.write_files(&[("/.mount-forbidden", b"forbidden")]),
            fs.create_dir("/.mount-forbidden"),
            fs.remove_file("/marker"),
            fs.remove_dir_all("/child"),
            fs.rename("/marker", "/.other-forbidden"),
        ];
        for result in results {
            assert_eq!(result.unwrap_err().err_no(), libc::EROFS as u32);
        }
        assert_eq!(
            fs.readv_with_options(
                ["/marker"]
                    .iter()
                    .map(crate::ReadOp::whole)
                    .collect::<Vec<_>>(),
                crate::ReadOptions::default()
            )
            .unwrap()
            .into_iter()
            .map(|r| r.data.unwrap())
            .collect::<Vec<_>>(),
            [b"unchanged".to_vec()]
        );
        assert!(fs.metadata("/child").unwrap().is_dir());
        // The fixture has a known shape. Teardown need not exercise kernel
        // recursive READDIR, unrelated to mount discovery/read-only policy.
        std::fs::remove_file(fixture.join("marker")).unwrap();
        std::fs::remove_dir(fixture.join("child")).unwrap();
        std::fs::remove_dir(fixture).unwrap();
    }
}
#[cfg(test)]
use crate::FsExt;
