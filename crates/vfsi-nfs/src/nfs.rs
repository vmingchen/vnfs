//! NFSv4.1 implementation of the vectorized [`VectorBackend`] API.
//!
//! [`NfsVecFs`] is the analog of the C `tc_init()` module handle: it connects
//! to an NFSv4.1 server, coalesces vector operations into as few compounds as
//! the server supports, and destroys its session/clientid on drop.

// bindgen emits lowercase constants (e.g. nfs_ftype4_NF4DIR) matched here.
#![allow(non_upper_case_globals)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use vfsi_sync::backend::{HandleBackend, VectorBackend};

use nfsv41_sys::*;
use vfsi_core::internal::ManyResults;
#[cfg(feature = "test-faults")]
use vfsi_core::internal::faults::{FaultInjector, OpenFaultPoint};

use crate::client::{FileHandle as WireFileHandle, NfsClient, OpenCreate};
use crate::path::{
    components_bytes, join_path_bytes, normalize_bytes, path_bytes, path_from_bytes,
    split_path_bytes,
};
use crate::session::make_verifier;
#[path = "read_pool.rs"]
mod read_pool;
pub use read_pool::{NfsReadPool, NfsReadPoolOptions};
// Re-export the shared types/trait so `use vnfs::legacy::nfs::*` works.
pub use crate::rpc::NfsAuthentication;
#[cfg(feature = "rpcsec-gss")]
pub use crate::rpc::RpcsecGssProtection;
pub use crate::vecfs::*;

/// An open file on the NFS server: resolved handle, open stateid, the current
/// read/write offset (for `tc_fseek`), and whether it was opened with
/// `O_APPEND` (writes then always go to the end of the file).
#[derive(Debug, Clone)]
struct OpenFile {
    fh: WireFileHandle,
    stateid: stateid4,
    cur_offset: u64,
    append: bool,
    /// Root-relative absolute path and non-destructive reopen flags. Internal
    /// temporary descriptors deliberately have no reopen recipe.
    reopen: Option<ReopenFile>,
}

#[derive(Debug, Clone)]
struct ReopenFile {
    path: PathBuf,
    flags: i32,
    mode: u32,
}

fn resource_status(status: u32) -> bool {
    matches!(
        status,
        nfsstat4_NFS4ERR_RESOURCE | nfsstat4_NFS4ERR_TOO_MANY_OPS
    )
}

#[derive(Debug, Clone)]
struct ConnectionConfig {
    host: String,
    root: PathBuf,
    minorversion: Option<u32>,
    connect_timeout: Duration,
    request_timeout: Duration,
    authentication: NfsAuthentication,
    client_owner: Option<Vec<u8>>,
    /// Stable for the lifetime of this client, including reconnects. A
    /// changed verifier tells an NFS server that the client rebooted.
    client_verifier: verifier4,
}

#[bitfields::bitfield(u8)]
#[derive(PartialEq, Eq)]
struct ConnectFlags {
    #[bits(default = true)]
    auto_reconnect: bool,
    #[bits(7)]
    _reserved: u8,
}

/// Options for establishing an NFSv4 connection.
///
/// The default uses automatic NFSv4.2-to-v4.1 negotiation, ten seconds for
/// setup, five seconds per RPC, and AUTH_SYS. Enable the `rpcsec-gss` Cargo
/// feature and select `NfsAuthentication::RpcsecGss` to opt into Kerberos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfsConnectOptions {
    /// Root of the application-visible namespace within the NFS pseudo-root.
    pub root: PathBuf,
    pub minorversion: Option<u32>,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub authentication: NfsAuthentication,
    /// Stable NFS client-owner identity. Must be unique among simultaneously
    /// active clients using the same server and credential.
    pub client_owner: Option<Vec<u8>>,
    pub recovery_policy: NfsRecoveryPolicy,
    flags: ConnectFlags,
    /// Client-side compound payload cap; None retains the conservative default.
    pub max_compound_bytes: Option<std::num::NonZeroUsize>,
}

impl Default for NfsConnectOptions {
    fn default() -> Self {
        Self {
            root: PathBuf::from("/"),
            minorversion: None,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(5),
            authentication: NfsAuthentication::AuthSys,
            client_owner: None,
            recovery_policy: NfsRecoveryPolicy::default(),
            flags: ConnectFlags::new(),
            max_compound_bytes: None,
        }
    }
}

impl NfsConnectOptions {
    /// Configure automatic read-only recovery.
    pub fn auto_reconnect(mut self, enabled: bool) -> Self {
        self.flags.set_auto_reconnect(enabled);
        self
    }
    pub const fn reconnects_automatically(&self) -> bool {
        self.flags.auto_reconnect()
    }
}

/// Builder for a completely configured NFS client.
#[derive(Clone)]
pub struct NfsClientBuilder {
    host: String,
    options: NfsConnectOptions,
    observer: Option<Arc<dyn NfsObserver>>,
    #[cfg(target_os = "linux")]
    mount: Option<crate::mount::NfsMount>,
}

impl std::fmt::Debug for NfsClientBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NfsClientBuilder")
            .field("host", &self.host)
            .field("options", &self.options)
            .field("observer", &self.observer.as_ref().map(|_| "configured"))
            .finish()
    }
}

impl NfsClientBuilder {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            options: NfsConnectOptions::default(),
            observer: None,
            #[cfg(target_os = "linux")]
            mount: None,
        }
    }

    /// Discover and pin a direct connection from a Linux NFS-mounted directory.
    #[cfg(target_os = "linux")]
    pub fn from_mount(path: impl AsRef<Path>) -> VfResult<Self> {
        Self::from_mount_config(crate::mount::NfsMount::discover(path)?)
    }

    /// Use a previously discovered mount for independent pooled connections.
    #[cfg(target_os = "linux")]
    pub fn from_mount_config(mount: crate::mount::NfsMount) -> VfResult<Self> {
        mount.check_local()?;
        let mut builder = Self::new(mount.host())
            .root(mount.root())
            .minor_version(Some(mount.minor_version()));
        builder.mount = Some(mount);
        Ok(builder)
    }

    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.options.root = root.into();
        self
    }

    pub fn minor_version(mut self, version: Option<u32>) -> Self {
        self.options.minorversion = version;
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.options.connect_timeout = timeout;
        self
    }

    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.options.request_timeout = timeout;
        self
    }

    pub fn authentication(mut self, authentication: NfsAuthentication) -> Self {
        self.options.authentication = authentication;
        self
    }

    pub fn client_owner(mut self, owner: impl Into<Vec<u8>>) -> Self {
        self.options.client_owner = Some(owner.into());
        self
    }

    pub fn recovery_policy(mut self, policy: NfsRecoveryPolicy) -> Self {
        self.options.recovery_policy = policy;
        self
    }

    pub fn auto_reconnect(mut self, enabled: bool) -> Self {
        self.options.flags.set_auto_reconnect(enabled);
        self
    }

    pub fn max_compound_bytes(mut self, bytes: usize) -> Self {
        self.options.max_compound_bytes = std::num::NonZeroUsize::new(bytes);
        self
    }

    pub fn observer(mut self, observer: Arc<dyn NfsObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Connect a reusable bounded pool for ordered pipelined large-file reads.
    pub fn connect_read_pool(self, options: NfsReadPoolOptions) -> VfResult<NfsReadPool> {
        self.validate()?;
        NfsReadPool::connect(self, options)
    }

    /// Connect independent sessions for concurrent, caller-distributed workloads.
    /// Each member still serializes its own operations; a vector call is never
    /// split across members. At most 64 sessions may be requested.
    pub fn connect_pool(self, size: usize) -> VfResult<Vec<NfsVecFs>> {
        self.validate()?;
        if !(1..=64).contains(&size) {
            return Err(
                VfError::client(0, ERR_INVAL).with_context("connect_pool", Path::new(&self.host))
            );
        }
        let pool_id = NEXT_CLIENT_POOL_ID.fetch_add(1, Ordering::Relaxed);
        (0..size)
            .map(|member| self.pool_member(pool_id, member).connect())
            .collect()
    }

    fn pool_member(&self, pool_id: u64, member: usize) -> Self {
        let mut builder = self.clone();
        if let Some(owner) = &mut builder.options.client_owner {
            let suffix = format!("-pool-{}-{pool_id:x}-{member}", std::process::id());
            owner.truncate(1024usize.saturating_sub(suffix.len()));
            owner.extend_from_slice(suffix.as_bytes());
        }
        builder
    }

    pub fn connect(self) -> VfResult<NfsVecFs> {
        self.validate()?;
        let mut filesystem = NfsVecFs::connect_with_options(&self.host, self.options)?;
        #[cfg(target_os = "linux")]
        if let Some(mount) = &self.mount {
            mount.verify(&mut filesystem)?;
            filesystem.read_only = mount.read_only();
            filesystem.mount_source = Some(mount.clone());
        }
        filesystem.observer = self.observer;
        filesystem.notify(NfsEvent::Connected {
            minor_version: filesystem.minorversion(),
        });
        // Construction has not installed the backend behind a client lock.
        for callback in HandleBackend::take_notifications(&mut filesystem) {
            callback();
        }
        Ok(filesystem)
    }

    fn validate(&self) -> VfResult<()> {
        #[cfg(target_os = "linux")]
        if let Some(mount) = &self.mount {
            mount.validate_options(&self.host, &self.options)?;
        }
        if self
            .options
            .client_owner
            .as_ref()
            .is_some_and(|owner| owner.is_empty() || owner.len() > 1024)
        {
            return Err(
                VfError::client(0, ERR_INVAL).with_context("connect", Path::new(&self.host))
            );
        }
        Ok(())
    }
}

/// Operational lifecycle events emitted outside the transport hot path.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum NfsEvent {
    Connected { minor_version: u32 },
    ReconnectStarted,
    ReconnectSucceeded,
    ReconnectFailed { error: VfError },
    Shutdown { result: Result<(), VfError> },
}

/// Observer hook which does not impose a logging or metrics framework.
/// Owned clients deliver events after releasing their backend lock, allowing
/// reentrant client operations. Callbacks run synchronously and should be short.
/// Owned-client delivery isolates callback panics from filesystem results.
/// Store weak client references to avoid an observer/client ownership cycle.
/// Direct backend users must drain `HandleBackend::take_notifications` outside locks.
pub trait NfsObserver: Send + Sync + 'static {
    fn on_event(&self, event: &NfsEvent);
}

/// Bounded recovery policy for side-effect-free operations.
///
/// A transport/session failure reconnects once at the operation layer. Each
/// reconnect may make several attempts so a restarting server can leave its
/// grace period. Mutating operations are never replayed automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NfsRecoveryPolicy {
    /// Hard cap on connection/session creation attempts, including the first.
    reconnect_attempts: usize,
    /// Delay after the first unsuccessful reconnect.
    initial_backoff: Duration,
    /// Maximum delay between attempts.
    max_backoff: Duration,
    /// Total retry window. One attempt is still made when this is zero.
    max_elapsed: Duration,
}

impl Default for NfsRecoveryPolicy {
    fn default() -> Self {
        Self {
            // The elapsed-time cap is the primary bound. The attempt cap
            // prevents a tight loop when failures return immediately.
            reconnect_attempts: 128,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
            max_elapsed: Duration::from_secs(120),
        }
    }
}

impl NfsRecoveryPolicy {
    pub fn new() -> Self {
        Self::default()
    }
    /// Set the attempt cap, including the first attempt. Zero also makes one attempt.
    pub fn reconnect_attempts(mut self, value: usize) -> Self {
        self.reconnect_attempts = value.max(1);
        self
    }
    pub const fn attempt_limit(self) -> usize {
        self.reconnect_attempts
    }
    /// Set the initial delay; execution clamps it to the maximum delay and remaining window.
    pub fn initial_backoff(mut self, value: Duration) -> Self {
        self.initial_backoff = value;
        self
    }
    pub const fn initial_delay(self) -> Duration {
        self.initial_backoff
    }
    /// Set the maximum delay; zero disables sleeping between attempts.
    pub fn max_backoff(mut self, value: Duration) -> Self {
        self.max_backoff = value;
        self
    }
    pub const fn maximum_delay(self) -> Duration {
        self.max_backoff
    }
    /// Set the retry window; zero still permits the first attempt.
    pub fn max_elapsed(mut self, value: Duration) -> Self {
        self.max_elapsed = value;
        self
    }
    pub const fn retry_window(self) -> Duration {
        self.max_elapsed
    }
}

/// Per-connection telemetry for NFSv4.2 server-side COPY.
///
/// `requests` counts COPY compounds sent to the server, `operations` counts
/// COPY operations acknowledged successfully, and `fallbacks` counts runtime
/// downgrades to the client-side implementation after the server rejected
/// COPY.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NfsServerCopyStats {
    pub requests: u64,
    pub operations: u64,
    pub fallbacks: u64,
}

/// An NFSv4 client exposing the vectorized [`VectorBackend`] API.
pub struct NfsVecFs {
    nfs: NfsClient,
    connection: ConnectionConfig,
    recovery_policy: NfsRecoveryPolicy,
    auto_reconnect: bool,
    recovery_in_progress: bool,
    cwd: PathBuf,
    next_fd: i32,
    /// Canonical open-file state, keyed by the client-assigned descriptor.
    open_files: std::collections::HashMap<i32, OpenFile>,
    /// Directory handles returned by `open_dir`, keyed by client-assigned id.
    open_dirs: std::collections::HashMap<i32, WireFileHandle>,
    /// Distinguishes directory descriptors from those of another client.
    dir_owner: u64,
    /// Handles no longer visible to callers whose CLOSE was not confirmed.
    deferred_descriptor_closes: Vec<OpenFile>,
    server_copy_enabled: bool,
    server_copy_stats: NfsServerCopyStats,
    /// How path-based bulk I/O is issued: one compound per batch including
    /// CLOSE (Ganesha's special-stateid behavior), one open+I/O compound
    /// plus a separate CLOSE compound (portable), or the old phased path.
    merged_mode: MergedIoMode,
    configured_max_compound_bytes: usize,
    /// A direct connection inferred from a read-only mount must not bypass it.
    read_only: bool,
    #[cfg(target_os = "linux")]
    mount_source: Option<crate::mount::NfsMount>,
    observer: Option<Arc<dyn NfsObserver>>,
    pending_events: Vec<NfsEvent>,
    #[cfg(feature = "test-faults")]
    fault_injector: Option<Arc<dyn FaultInjector>>,
    #[cfg(feature = "test-faults")]
    short_read_once: Option<(usize, usize)>,
    #[cfg(feature = "test-faults")]
    short_write_once: Option<usize>,
}

static NEXT_DIR_OWNER: AtomicU64 = AtomicU64::new(1);
static NEXT_CLIENT_POOL_ID: AtomicU64 = AtomicU64::new(1);

/// Which merged-compound strategy the NFS backend uses for path-based bulk
/// I/O, downgraded automatically when the server rejects the current form.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MergedIoMode {
    /// `[PUTROOTFH, LOOKUP, SAVEFH, OPEN, WRITE, RESTOREFH, ..., CLOSE]` in a
    /// single compound (special-stateid CLOSE; Ganesha).
    Full,
    /// One open+I/O compound, then a separate CLOSE compound with the real
    /// stateids (portable).
    OpenWrite,
    /// Legacy phased path: resolve + probe + open + I/O + close compounds.
    Off,
}

fn check_mount_write(read_only: bool, count: usize) -> VfResult<()> {
    if read_only && count != 0 {
        Err(VfError::client(0, libc::EROFS as u32))
    } else {
        Ok(())
    }
}

impl NfsVecFs {
    fn ensure_writable(&self, count: usize) -> VfResult<()> {
        check_mount_write(self.read_only, count)
    }

    pub fn builder(host: impl Into<String>) -> NfsClientBuilder {
        NfsClientBuilder::new(host)
    }

    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn set_fault_injector(&mut self, injector: Arc<dyn FaultInjector>) {
        self.nfs.set_fault_injector(injector.clone());
        self.fault_injector = Some(injector);
    }

    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_open_handle_count(&self) -> usize {
        self.open_files.len()
    }

    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_limit_compound_operations(&mut self, limit: usize) -> usize {
        self.nfs.test_limit_compound_operations(limit)
    }

    /// Simulate a short successful READ reply at the descriptor boundary.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_short_read_once(&mut self, bytes: usize) {
        self.test_short_read_for_request_once(0, bytes);
    }

    /// Shorten the first wire reply belonging to the selected logical READ.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_short_read_for_request_once(&mut self, index: usize, bytes: usize) {
        self.short_read_once = Some((index, bytes));
    }

    /// Send only a prefix of the next descriptor WRITE to the real server.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_short_write_once(&mut self, bytes: usize) {
        self.short_write_once = Some(bytes);
    }

    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_io_chunk_bytes(&self) -> usize {
        self.nfs.per_op_bytes()
    }

    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_deferred_descriptor_close_count(&self) -> usize {
        self.deferred_descriptor_closes.len()
    }

    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_confirmed_path_closes(&self) -> usize {
        self.nfs.confirmed_path_closes()
    }

    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_deferred_path_close_count(&self) -> usize {
        self.nfs.deferred_path_close_count()
    }

    #[cfg(feature = "test-faults")]
    fn inject_open_fault(&self, point: OpenFaultPoint) -> VfResult<()> {
        self.fault_injector
            .as_ref()
            .map_or(Ok(()), |injector| injector.check(&point))
    }
    /// Force a particular merged-compound mode (diagnostics/tests): "full"
    /// (default, one compound incl. CLOSE), "openwrite" (open+I/O compound +
    /// separate close), or "off" (legacy phased path).
    #[doc(hidden)]
    pub fn set_merged_mode(&mut self, mode: &str) {
        self.merged_mode = match mode {
            "openwrite" => MergedIoMode::OpenWrite,
            "off" => MergedIoMode::Off,
            _ => MergedIoMode::Full,
        };
    }

    /// Set the per-compound payload cap for merged path I/O (bytes; 0 =
    /// unlimited).
    pub fn set_max_compound_bytes(&mut self, bytes: usize) {
        self.configured_max_compound_bytes = bytes;
        self.nfs.set_max_compound_bytes(bytes);
    }

    /// Negotiated NFS minor version for this connection.
    pub fn minorversion(&self) -> u32 {
        self.nfs.minorversion()
    }

    /// Whether this connection will currently attempt NFSv4.2 server COPY.
    /// The value becomes false if the server rejects COPY at runtime.
    pub fn server_copy_enabled(&self) -> bool {
        self.server_copy_enabled
    }

    /// Return server-side COPY activity for this connection.
    pub fn server_copy_stats(&self) -> NfsServerCopyStats {
        self.server_copy_stats
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod filesystem_stats_tests;

mod handle_backend;
mod open;
mod recovery;
mod vector_backend;
use open::*;
mod io;
use io::*;
mod metadata;
#[cfg(feature = "fuzzing")]
pub(crate) use metadata::validate_attr_list;
use metadata::*;
mod directory;
use directory::*;
mod namespace;
use namespace::*;
mod copy;
