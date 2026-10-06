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
            #[cfg(target_os = "linux")]
            mount_binding: None,
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

    /// Build a client from this already-discovered mount configuration.
    ///
    /// This avoids rediscovering the mount when an application first inspects
    /// it (for example, to group operands) and then opens a direct connection.
    pub fn builder(&self) -> VfResult<NfsBuilder> {
        NfsBuilder::from_mount_config(self)
    }
}

/// NFS configuration builder whose `connect` returns an owned native client.
#[derive(Debug, Clone)]
pub struct NfsBuilder {
    inner: NfsClientBuilder,
    limits: ResourceLimits,
    // A customization callback may clone the supplied builder, but must not
    // replace it with one whose backend lacks the original mount safeguards.
    #[cfg(target_os = "linux")]
    mount_binding: Option<Arc<()>>,
}

impl NfsBuilder {
    /// Discover mount configuration, then customize timeouts and other tuning.
    pub fn from_mount(path: impl AsRef<Path>) -> VfResult<Self> {
        #[cfg(target_os = "linux")]
        {
            Self::from_mount_config(&Nfs::discover_mount(path)?)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(crate::Error::client(0, libc::EOPNOTSUPP as u32)
                .with_context("from_mount requires Linux", path.as_ref()))
        }
    }

    /// Configure a client from previously discovered mount information.
    ///
    /// The mount is revalidated when the client connects, so stale mount
    /// configuration fails rather than silently targeting a different export.
    #[cfg(target_os = "linux")]
    fn from_mount_config(mount: &NfsMount) -> VfResult<Self> {
        Ok(Self {
            inner: NfsClientBuilder::from_mount_config(mount.inner.clone())?,
            limits: ResourceLimits::default(),
            mount_binding: Some(Arc::new(())),
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn configure_for_mount(
        self,
        local_path: &Path,
        configure: impl FnOnce(Self) -> Self,
    ) -> VfResult<Self> {
        let binding = self.mount_binding.clone();
        let configured = configure(self);
        if !binding
            .as_ref()
            .zip(configured.mount_binding.as_ref())
            .is_some_and(|(expected, actual)| Arc::ptr_eq(expected, actual))
        {
            return Err(crate::Error::client(0, libc::EINVAL as u32).with_context(
                "mount session customization must retain the supplied builder",
                local_path,
            ));
        }
        Ok(configured)
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
    fn mount_customization_rejects_unbound_and_differently_bound_builders() {
        let path = Path::new("/mnt/nfs/project");
        for separately_bound in [false, true] {
            let mut supplied = Nfs::builder("127.0.0.1:2049");
            supplied.mount_binding = Some(Arc::new(()));
            let mut replacement = Nfs::builder("127.0.0.1:2049");
            if separately_bound {
                replacement.mount_binding = Some(Arc::new(()));
            }
            let error = supplied
                .configure_for_mount(path, |_| replacement)
                .unwrap_err();
            assert_eq!(error.err_no(), libc::EINVAL as u32);
            assert_eq!(error.path(), Some(path));
            assert!(error.to_string().contains("supplied builder"));
        }
    }

    #[test]
    fn mount_customization_accepts_cloning_and_tuning_the_supplied_builder() {
        let binding = Arc::new(());
        let mut supplied = Nfs::builder("127.0.0.1:2049");
        supplied.mount_binding = Some(binding.clone());
        let limits = ResourceLimits {
            max_read_bytes: 1024,
            ..ResourceLimits::default()
        };
        let configured = supplied
            .configure_for_mount(Path::new("/mnt/nfs/project"), |builder| {
                builder
                    .clone()
                    .request_timeout(Duration::from_secs(2))
                    .limits(limits)
            })
            .unwrap();
        assert!(Arc::ptr_eq(
            configured.mount_binding.as_ref().unwrap(),
            &binding
        ));
        assert_eq!(configured.limits, limits);
    }

    #[test]
    #[ignore = "requires configured NFS mount fixtures"]
    fn live_mount_session_rejects_replacement_builders() {
        let directory = std::env::var("VFSI_NFS_TEST_MOUNT")
            .expect("VFSI_NFS_TEST_MOUNT is required for this ignored integration test");
        let mount = Nfs::discover_mount(&directory).unwrap();
        let host = mount.host().to_owned();
        let replacement = |_| {
            Nfs::builder(&host)
                .root("/")
                .version(NfsVersion::V4_2)
                .connect_timeout(Duration::from_secs(2))
                .request_timeout(Duration::from_secs(2))
        };
        let error =
            crate::helpers::NfsMountSession::from_discovered_with(mount, replacement).unwrap_err();
        assert_eq!(error.err_no(), libc::EINVAL as u32);
        assert!(error.to_string().contains("supplied builder"));

        let error =
            crate::helpers::NfsMountSession::from_mount_with(&directory, replacement).unwrap_err();
        assert_eq!(error.err_no(), libc::EINVAL as u32);
        assert!(error.to_string().contains("supplied builder"));
    }

    #[test]
    #[ignore = "requires configured NFS mount fixtures"]
    fn live_mount_constructor_roots_vector_io_at_a_subdirectory() {
        let mount = std::env::var("VFSI_NFS_TEST_MOUNT")
            .expect("VFSI_NFS_TEST_MOUNT is required for this ignored integration test");
        let directory = Path::new(&mount).join(format!(".vnfs-from-mount-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let session = crate::helpers::NfsMountSession::from_mount_with(&directory, |builder| {
            builder.request_timeout(Duration::from_secs(2))
        })
        .unwrap();
        assert_eq!(session.local_root(), directory.canonicalize().unwrap());
        assert_eq!(
            session
                .map(
                    directory.join("file-1"),
                    crate::helpers::ResolvePath::NoFollow
                )
                .unwrap(),
            Path::new("/file-1")
        );
        let fs = session.fs();
        fs.write_files(&[
            ("/file-1", b"hello".as_slice()),
            ("/file-2", b"world".as_slice()),
        ])
        .unwrap();
        assert_eq!(
            fs.vread(
                ["/file-1", "/file-2"]
                    .iter()
                    .map(crate::ReadOp::whole)
                    .collect::<Vec<_>>(),
                crate::ReadOptions::default()
            )
            .unwrap()
            .into_iter()
            .map(|r| r.into_data().unwrap())
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
        assert!(fs.attrs("/").unwrap().is_dir());
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
            fs.vread(
                ["/marker"]
                    .iter()
                    .map(crate::ReadOp::whole)
                    .collect::<Vec<_>>(),
                crate::ReadOptions::default()
            )
            .unwrap()
            .into_iter()
            .map(|r| r.into_data().unwrap())
            .collect::<Vec<_>>(),
            [b"unchanged".to_vec()]
        );
        assert!(fs.attrs("/child").unwrap().is_dir());
        // The fixture has a known shape. Teardown need not exercise kernel
        // recursive READDIR, unrelated to mount discovery/read-only policy.
        std::fs::remove_file(fixture.join("marker")).unwrap();
        std::fs::remove_dir(fixture.join("child")).unwrap();
        std::fs::remove_dir(fixture).unwrap();
    }
}
#[cfg(test)]
use crate::VfsiExt;
