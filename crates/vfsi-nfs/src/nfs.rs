//! NFSv4.1 implementation of the vectorized [`VecFs`] API.
//!
//! [`NfsVecFs`] is the analog of the C `tc_init()` module handle: it connects
//! to an NFSv4.1 server, coalesces vector operations into as few compounds as
//! the server supports, and destroys its session/clientid on drop.

// bindgen emits lowercase constants (e.g. nfs_ftype4_NF4DIR) matched here.
#![allow(non_upper_case_globals)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nfsv41_sys::*;
use vfsi_core::internal::ManyResults;
#[cfg(feature = "test-faults")]
use vfsi_core::internal::faults::{FaultInjector, OpenFaultPoint};

use crate::client::{FileHandle, NfsClient, OpenCreate};
use crate::path::{
    components_bytes, join_path_bytes, normalize_bytes, path_bytes, path_from_bytes,
    split_path_bytes,
};
use crate::session::make_verifier;
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
    fh: FileHandle,
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
    pub auto_reconnect: bool,
    /// Client-side compound payload cap; zero uses the negotiated server cap.
    pub max_compound_bytes: usize,
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
            auto_reconnect: true,
            max_compound_bytes: 0,
        }
    }
}

/// Builder for a completely configured NFS client.
#[derive(Clone)]
pub struct NfsClientBuilder {
    host: String,
    options: NfsConnectOptions,
    observer: Option<Arc<dyn NfsObserver>>,
    require_secure_authentication: bool,
}

impl std::fmt::Debug for NfsClientBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NfsClientBuilder")
            .field("host", &self.host)
            .field("options", &self.options)
            .field("observer", &self.observer.as_ref().map(|_| "configured"))
            .field(
                "require_secure_authentication",
                &self.require_secure_authentication,
            )
            .finish()
    }
}

impl NfsClientBuilder {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            options: NfsConnectOptions::default(),
            observer: None,
            require_secure_authentication: false,
        }
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
        self.options.auto_reconnect = enabled;
        self
    }

    pub fn max_compound_bytes(mut self, bytes: usize) -> Self {
        self.options.max_compound_bytes = bytes;
        self
    }

    pub fn observer(mut self, observer: Arc<dyn NfsObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Refuse to connect with AUTH_SYS. This is a fail-closed guard for
    /// deployments which require cryptographic peer authentication.
    pub fn require_secure_authentication(mut self, required: bool) -> Self {
        self.require_secure_authentication = required;
        self
    }

    pub fn connect(self) -> VfResult<NfsVecFs> {
        if self.require_secure_authentication
            && matches!(&self.options.authentication, NfsAuthentication::AuthSys)
        {
            return Err(
                VfError::client(0, ERR_ACCES).with_context("connect", Path::new(&self.host))
            );
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
        let mut filesystem = NfsVecFs::connect_with_options(&self.host, self.options)?;
        filesystem.observer = self.observer;
        filesystem.notify(NfsEvent::Connected {
            minor_version: filesystem.minorversion(),
        });
        Ok(filesystem)
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
    pub reconnect_attempts: usize,
    /// Delay after the first unsuccessful reconnect.
    pub initial_backoff: Duration,
    /// Maximum delay between attempts.
    pub max_backoff: Duration,
    /// Total retry window. One attempt is still made when this is zero.
    pub max_elapsed: Duration,
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

/// An NFSv4 client exposing the vectorized [`VecFs`] API.
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
    /// Handles no longer visible to callers whose CLOSE was not confirmed.
    deferred_descriptor_closes: Vec<OpenFile>,
    server_copy_enabled: bool,
    server_copy_stats: NfsServerCopyStats,
    /// How path-based bulk I/O is issued: one compound per batch including
    /// CLOSE (Ganesha's special-stateid behavior), one open+I/O compound
    /// plus a separate CLOSE compound (portable), or the old phased path.
    merged_mode: MergedIoMode,
    configured_max_compound_bytes: usize,
    observer: Option<Arc<dyn NfsObserver>>,
    #[cfg(feature = "test-faults")]
    fault_injector: Option<Arc<dyn FaultInjector>>,
}

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

fn remap_descriptor_chunk_error(error: VfError, start: usize, owners: &[usize]) -> VfError {
    error.index_opt().map_or(error.clone(), |local_index| {
        let chunk_index = start.saturating_add(local_index);
        let index = owners.get(chunk_index).copied().unwrap_or(chunk_index);
        error.with_index(index)
    })
}

fn remap_active_error(error: VfError, active: &[usize]) -> VfError {
    match error
        .index_opt()
        .and_then(|index| active.get(index))
        .copied()
    {
        Some(original) => error.with_index(original),
        None => error,
    }
}

fn bounded_read_allv_batch(
    active: &[usize],
    remaining: usize,
    compound_budget: usize,
    max_window: usize,
) -> (Vec<usize>, usize) {
    let cohort_len = if remaining == 0 {
        1
    } else {
        active.len().min(remaining)
    };
    let cohort = active[..cohort_len].to_vec();
    if remaining == 0 {
        // Once the payload budget is exhausted, probe one file at a time so
        // exact-limit EOF remains distinguishable from an oversized file
        // without allocating another cohort-sized response.
        return (cohort, 1);
    }
    let protocol_window = (compound_budget / cohort_len)
        .saturating_sub(128)
        .min(max_window)
        .max(1);
    let allocation_window = (remaining / cohort_len).max(1);
    (cohort, protocol_window.min(allocation_window))
}

#[allow(clippy::too_many_arguments)]
fn append_bounded_walk_page(
    root: &Path,
    dir: &Path,
    masks: AttrMask,
    ids: &[u32],
    page: &[crate::client::DirEntry],
    options: WalkOptions,
    entry_count: &mut usize,
    path_bytes: &mut usize,
    out: &mut Vec<VfAttrs>,
) -> VfResult<()> {
    for entry in page {
        if *entry_count >= options.entry_limit() {
            return Err(
                VfError::failure(*entry_count, libc::EFBIG as u32).with_context("walk", dir)
            );
        }
        let path = dir.join(path_from_bytes(&entry.name));
        let next_path_bytes = path_bytes
            .checked_add(path.as_os_str().len())
            .filter(|bytes| *bytes <= options.path_byte_limit())
            .ok_or_else(|| {
                VfError::failure(*entry_count, libc::EFBIG as u32).with_context("walk", dir)
            })?;
        let mut attrs = VfAttrs {
            file: VfFile::from_os_path(&path),
            masks,
            ..VfAttrs::default()
        };
        let values = parse_attr_list(ids, &entry.attrs)
            .map_err(|error| error.with_index(*entry_count).with_context("walk", dir))?;
        apply_attrs(&mut attrs, &values);
        if attrs.ftype == VfType::Directory {
            let depth = path
                .strip_prefix(root)
                .map(|relative| relative.components().count())
                .unwrap_or(usize::MAX);
            if depth > options.depth_limit() && !options.truncates_at_depth_limit() {
                return Err(
                    VfError::failure(*entry_count, libc::EFBIG as u32).with_context("walk", dir)
                );
            }
        }
        out.push(attrs);
        *path_bytes = next_path_bytes;
        *entry_count += 1;
    }
    Ok(())
}

fn merge_read_allv_round(
    active: &[usize],
    results: &[ReadResult],
    out: &mut [Vec<u8>],
    offsets: &mut [u64],
    total: &mut usize,
    max_total_bytes: usize,
) -> VfResult<Vec<usize>> {
    if results.len() != active.len() {
        return Err(VfError::transport(
            None,
            format!(
                "NFS readv returned {} results for {} active files",
                results.len(),
                active.len()
            ),
        ));
    }
    let mut next = Vec::with_capacity(active.len());
    for (result, &original) in results.iter().zip(active) {
        if result.data.is_empty() && !result.eof {
            return Err(VfError::transport(
                original,
                "NFS READ made no progress without reporting EOF",
            ));
        }
        *total = total
            .checked_add(result.data.len())
            .filter(|size| *size <= max_total_bytes)
            .ok_or_else(|| VfError::failure(original, libc::EFBIG as u32))?;
        out[original].extend_from_slice(&result.data);
        offsets[original] = result
            .offset
            .checked_add(result.data.len() as u64)
            .ok_or_else(|| VfError::failure(original, libc::EOVERFLOW as u32))?;
        if !result.eof {
            next.push(original);
        }
    }
    Ok(next)
}

fn non_destructive_reopen_flags(flags: i32) -> i32 {
    flags & !(libc::O_CREAT | libc::O_EXCL | libc::O_TRUNC)
}

/// Root-relative application path for `path`, per the `VecFs::abs_path`
/// contract: absolute inputs are taken relative to the application root and
/// relative inputs resolve against `cwd`; neither includes the export prefix.
fn namespace_path(cwd: &Path, path: &Path) -> PathBuf {
    let namespace_relative = if path.is_absolute() {
        path.strip_prefix("/").unwrap_or(path).to_path_buf()
    } else {
        cwd.join(path)
    };
    path_from_bytes(&normalize_bytes(path_bytes(&namespace_relative)))
}

/// Export-root path used for NFS resolution.
fn server_path_for(root: &Path, cwd: &Path, path: &Path) -> PathBuf {
    root.join(namespace_path(cwd, path))
}

fn adb_block_base(pattern: &Adb, block: usize, index: usize) -> VfResult<u64> {
    let relative = (block as u64)
        .checked_mul(pattern.adb_block_size)
        .ok_or_else(|| VfError::failure(index, libc::EOVERFLOW as u32))?;
    pattern
        .adb_offset
        .checked_add(relative)
        .ok_or_else(|| VfError::failure(index, libc::EOVERFLOW as u32))
}

fn adb_field_offset(base: u64, relative: u64, index: usize) -> VfResult<u64> {
    base.checked_add(relative)
        .ok_or_else(|| VfError::failure(index, libc::EOVERFLOW as u32))
}

impl NfsVecFs {
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

    /// Close all descriptors and explicitly tear down NFS session state.
    /// `Drop` remains a best-effort fallback when this result is not needed.
    pub fn shutdown(mut self) -> VfResult<()> {
        let observer = self.observer.clone();
        let closes: Vec<crate::client::CloseOp> = self
            .open_files
            .drain()
            .map(|(_, open)| open)
            .chain(self.deferred_descriptor_closes.drain(..))
            .map(|open| crate::client::CloseOp {
                fh: open.fh,
                stateid: open.stateid,
            })
            .collect();
        let close_result = self
            .nfs
            .close_many(&closes)
            .map_err(VfError::from_rpc_indexed);
        let shutdown_result = self
            .nfs
            .shutdown()
            .map_err(|error| VfError::from_rpc(error, None));
        let result = close_result.and(shutdown_result);
        if let Some(observer) = observer {
            observer.on_event(&NfsEvent::Shutdown {
                result: result.clone(),
            });
        }
        result
    }

    fn notify(&self, event: NfsEvent) {
        if let Some(observer) = &self.observer {
            observer.on_event(&event);
        }
    }

    fn visible_path(&self, server_path: &Path) -> PathBuf {
        Path::new("/").join(
            server_path
                .strip_prefix(&self.connection.root)
                .unwrap_or(server_path),
        )
    }

    /// Server path (export root joined with the namespace-relative path) used
    /// for NFS resolution.
    fn server_path(&self, path: &Path) -> PathBuf {
        server_path_for(&self.connection.root, &self.cwd, path)
    }

    /// Server-path equivalent of [`VecFs::vf_path`]. Descriptors and the
    /// saved/cwd sentinels have no path.
    fn server_vf_path(&self, file: &VfFile) -> VfResult<PathBuf> {
        match file {
            VfFile::Path {
                base: VfPathBase::Abs,
                path,
            } => Ok(self.server_path(&Path::new("/").join(path))),
            VfFile::Path {
                base: VfPathBase::Cwd,
                path,
            }
            | VfFile::CwdPath(path) => Ok(self.server_path(path)),
            VfFile::Cwd => Ok(self.server_path(Path::new(""))),
            VfFile::Descriptor(_) | VfFile::Saved => Err(VfError::failure(0, ERR_INVAL)),
            _ => Err(VfError::failure(0, ERR_INVAL)),
        }
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

    /// Configure bounded automatic recovery for side-effect-free operations.
    pub fn set_recovery_policy(&mut self, policy: NfsRecoveryPolicy) {
        self.recovery_policy = policy;
    }

    /// Enable or disable automatic recovery of side-effect-free operations.
    pub fn set_auto_reconnect(&mut self, enabled: bool) {
        self.auto_reconnect = enabled;
    }

    /// Resolve `files` to file handles in as few compounds as possible: each
    /// unique parent directory is resolved once, then all children are
    /// LOOKUPed in tolerant batches (`[PUTFH, LOOKUP, GETFH, GETATTR type]`
    /// per child). Returns per-index `(handle, own type)`; a failed LOOKUP
    /// (e.g. NOENT) is reported per path. When `follow` is set, a
    /// final-component symlink is followed through the existing per-path
    /// resolver. Descriptors resolve straight from the open-file table.
    fn resolve_many_tcfile(
        &mut self,
        files: &[&VfFile],
        follow: bool,
    ) -> VfResult<Vec<Result<(FileHandle, u32), u32>>> {
        use std::collections::{BTreeMap, HashMap};
        // Group by parent directory.
        let mut groups: BTreeMap<Vec<u8>, Vec<(usize, Vec<u8>)>> = BTreeMap::new();
        let mut out: Vec<Result<(FileHandle, u32), u32>> =
            vec![Err(nfsstat4_NFS4ERR_NOENT); files.len()];
        for (i, f) in files.iter().enumerate() {
            if f.is_descriptor() {
                let fd = f.fd().unwrap();
                out[i] = match self.open_files.get(&fd) {
                    Some(o) => Ok((o.fh.clone(), 0)),
                    None => Err(ERR_EBADF),
                };
                continue;
            }
            let path = self.server_vf_path(f).map_err(|e| e.with_index(i))?;
            if path.as_os_str().is_empty() {
                // The export root itself.
                out[i] = Ok((self.nfs.root().clone(), nfs_ftype4_NF4DIR));
                continue;
            }
            let path_bytes = path_bytes(&path);
            let (dir, name) =
                split_path_bytes(path_bytes).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            groups.entry(dir).or_default().push((i, name));
        }
        let mut dir_cache: HashMap<Vec<u8>, FileHandle> = HashMap::new();
        for (dir, entries) in groups {
            let dirfh = match dir_cache.get(&dir) {
                Some(fh) => fh.clone(),
                None => match self.resolve_path(&path_from_bytes(&dir), true) {
                    Ok(fh) => {
                        dir_cache.insert(dir, fh.clone());
                        fh
                    }
                    Err(e) => {
                        let err = e.err_no();
                        for (i, _) in &entries {
                            out[*i] = Err(err);
                        }
                        continue;
                    }
                },
            };
            let ops: Vec<(FileHandle, Vec<u8>)> = entries
                .iter()
                .map(|(_, name)| (dirfh.clone(), name.clone()))
                .collect();
            let results = self
                .nfs
                .lookup_getattr_many(&ops)
                .map_err(|e| VfError::from_rpc(e, None))?;
            for ((i, _), r) in entries.iter().zip(results) {
                match r {
                    Ok((fh, ftype)) => {
                        if follow && ftype == nfs_ftype4_NF4LNK {
                            let full = self.server_vf_path(files[*i])?;
                            out[*i] = match self.resolve_follow(&full) {
                                Ok(fh) => Ok((fh, ftype)),
                                Err(e) => Err(e.err_no()),
                            };
                        } else {
                            out[*i] = Ok((fh, ftype));
                        }
                    }
                    Err(status) => out[*i] = Err(status),
                }
            }
        }
        Ok(out)
    }

    /// The merged (single-compound) setattrsv: resolve each parent once,
    /// then LOOKUP + GETATTR type + SETATTR per file. Symlinks (either to
    /// refuse for lsetattrsv or to follow for setattrsv) are handled
    /// per-file via the phased path.
    fn setattrsv_impl(&mut self, attrs: &[VfAttrs], follow: bool) -> VfRes {
        const SETTABLE: AttrMask = AttrMask::MODE
            .union(AttrMask::SIZE)
            .union(AttrMask::ATIME)
            .union(AttrMask::MTIME);
        if attrs.is_empty() {
            return Ok(());
        }
        for (i, a) in attrs.iter().enumerate() {
            let unsupported = a.masks.difference(SETTABLE);
            if !unsupported.is_empty() {
                return Err(VfError::unsupported(i));
            }
        }
        let mut ops = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            let file = if a.file.is_descriptor() {
                let fd = a.file.fd().unwrap();
                let open = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
                crate::client::FileRef::Handle(open.fh.clone())
            } else {
                crate::client::FileRef::Path(
                    crate::path::path_bytes(
                        &self.server_vf_path(&a.file).map_err(|e| e.with_index(i))?,
                    )
                    .to_vec(),
                )
            };
            let mode = if a.masks.contains(AttrMask::MODE) {
                Some(a.mode & 0o7777)
            } else {
                None
            };
            let size = if a.masks.contains(AttrMask::SIZE) {
                Some(a.size)
            } else {
                None
            };
            let atime = a
                .masks
                .contains(AttrMask::ATIME)
                .then_some((a.atime_sec, a.atime_nsec));
            let mtime = a
                .masks
                .contains(AttrMask::MTIME)
                .then_some((a.mtime_sec, a.mtime_nsec));
            ops.push(crate::client::PathSetattrOp {
                file,
                mode,
                size,
                atime,
                mtime,
                check_type: true,
            });
        }
        match self.nfs.setattr_path_compound(&ops) {
            Ok(outcome) => {
                if let Some((i, _st)) = outcome.failed {
                    // Prefix [0..i) succeeded; resume [i..] via the phased
                    // path, re-attributing its error to the original index.
                    for (k, a) in attrs.iter().enumerate().take(i) {
                        let own_type = outcome.types[k];
                        if own_type == Some(nfs_ftype4_NF4LNK) {
                            if !follow {
                                return Err(VfError::unsupported(k));
                            }
                            self.setattr_one_following(k, a)?;
                        }
                    }
                    if let Err(e) = self.setattrsv_phased(&attrs[i..], follow) {
                        return Err(e.map_index(|rel| i + rel));
                    }
                    return Ok(());
                }
                for (i, a) in attrs.iter().enumerate() {
                    let own_type = outcome.types[i];
                    if own_type == Some(nfs_ftype4_NF4LNK) {
                        if !follow {
                            // No non-following mode/size setter for symlinks.
                            return Err(VfError::unsupported(i));
                        }
                        self.setattr_one_following(i, a)?;
                    }
                }
                Ok(())
            }
            Err(error) if error.is_transport() => Err(VfError::from_rpc(error, None)),
            Err(_) => self.setattrsv_phased(attrs, follow),
        }
    }

    /// The legacy phased setattrsv (resolve_many_tcfile + setattr_many).
    fn setattrsv_phased(&mut self, attrs: &[VfAttrs], follow: bool) -> VfRes {
        const SETTABLE: AttrMask = AttrMask::MODE
            .union(AttrMask::SIZE)
            .union(AttrMask::ATIME)
            .union(AttrMask::MTIME);
        for (i, a) in attrs.iter().enumerate() {
            let unsupported = a.masks.difference(SETTABLE);
            if !unsupported.is_empty() {
                return Err(VfError::unsupported(i));
            }
        }
        let files: Vec<&VfFile> = attrs.iter().map(|a| &a.file).collect();
        let resolved = self.resolve_many_tcfile(&files, follow)?;
        let mut ops = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            let (fh, ftype) = match &resolved[i] {
                Ok(x) => x.clone(),
                Err(status) => return Err(VfError::nfs(i, *status)),
            };
            if !follow && ftype == nfs_ftype4_NF4LNK {
                // NFSv4 has no non-following mode/size setter for symlinks;
                // refuse like the `std::fs` backend instead of pretending the
                // SETATTR applied to the link.
                return Err(VfError::unsupported(i));
            }
            let mode = if a.masks.contains(AttrMask::MODE) {
                Some(a.mode & 0o7777)
            } else {
                None
            };
            let size = if a.masks.contains(AttrMask::SIZE) {
                Some(a.size)
            } else {
                None
            };
            let atime = a
                .masks
                .contains(AttrMask::ATIME)
                .then_some((a.atime_sec, a.atime_nsec));
            let mtime = a
                .masks
                .contains(AttrMask::MTIME)
                .then_some((a.mtime_sec, a.mtime_nsec));
            ops.push(crate::client::SetattrOp {
                fh,
                mode,
                size,
                atime,
                mtime,
            });
        }
        self.nfs
            .setattr_many(&ops)
            .map_err(VfError::from_rpc_indexed)
    }

    /// The merged (single-compound) getattrsv: resolve each parent once,
    /// then LOOKUP + GETATTR per file. Final-component symlinks (follow
    /// semantics) are resolved individually afterwards.
    fn getattrsv_impl(&mut self, attrs: &mut [VfAttrs], follow: bool) -> VfRes {
        if attrs.is_empty() {
            return Ok(());
        }
        let mut ops = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            let file = if a.file.is_descriptor() {
                let fd = a.file.fd().unwrap();
                let open = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
                crate::client::FileRef::Handle(open.fh.clone())
            } else {
                crate::client::FileRef::Path(
                    crate::path::path_bytes(
                        &self.server_vf_path(&a.file).map_err(|e| e.with_index(i))?,
                    )
                    .to_vec(),
                )
            };
            ops.push(crate::client::PathGetattrOp {
                file,
                attrs: request_mask_to_attr_list(&a.masks),
            });
        }
        match self.nfs.getattr_path_compound(&ops) {
            Ok(outcome) => {
                if let Some((i, _st)) = outcome.failed {
                    // Prefix [0..i) succeeded; resume [i..] via the phased
                    // path, re-attributing its error to the original index.
                    let mut prefix_attrs = attrs[..i].to_vec();
                    for (k, a) in prefix_attrs.iter_mut().enumerate() {
                        let list = outcome.lists[k].as_deref().unwrap_or_default();
                        let v = parse_attr_list(&ops[k].attrs, list)?;
                        apply_attrs(a, &v);
                        if follow && a.ftype == VfType::Symlink {
                            self.stat_one_following(k, a)?;
                        }
                    }
                    attrs[..i].clone_from_slice(&prefix_attrs);
                    if let Err(e) = self.getattrsv_phased(&mut attrs[i..], follow) {
                        return Err(e.map_index(|rel| i + rel));
                    }
                    return Ok(());
                }
                let mut symlinks = Vec::new();
                for (i, (a, op)) in attrs.iter_mut().zip(&ops).enumerate() {
                    let list = outcome.lists[i].as_deref().unwrap_or_default();
                    let v = parse_attr_list(&op.attrs, list)?;
                    apply_attrs(a, &v);
                    if follow && a.ftype == VfType::Symlink {
                        symlinks.push(i);
                    }
                }
                for i in symlinks {
                    self.stat_one_following(i, &mut attrs[i])?;
                }
                Ok(())
            }
            Err(error) if error.is_transport() => Err(VfError::from_rpc(error, None)),
            Err(_) => self.getattrsv_phased(attrs, follow),
        }
    }

    /// The legacy phased getattrsv (resolve_many_tcfile + getattr_many).
    fn getattrsv_phased(&mut self, attrs: &mut [VfAttrs], follow: bool) -> VfRes {
        let files: Vec<&VfFile> = attrs.iter().map(|a| &a.file).collect();
        let resolved = self.resolve_many_tcfile(&files, follow)?;
        let mut ops = Vec::with_capacity(attrs.len());
        let mut ids_list = Vec::with_capacity(attrs.len());
        let mut first_failure: Option<VfError> = None;
        for (i, a) in attrs.iter().enumerate() {
            let fh = match &resolved[i] {
                Ok((fh, _)) => fh.clone(),
                Err(status) => {
                    if first_failure.is_none() {
                        first_failure = Some(VfError::nfs(i, *status));
                    }
                    ids_list.push(Vec::new());
                    continue;
                }
            };
            let ids = request_mask_to_attr_list(&a.masks);
            ids_list.push(ids.clone());
            ops.push(crate::client::GetattrOp { fh, attrs: ids });
        }
        if let Some(e) = first_failure {
            return Err(e);
        }
        let results = self
            .nfs
            .getattr_many(&ops)
            .map_err(VfError::from_rpc_indexed)?;
        for ((a, ids), list) in attrs.iter_mut().zip(ids_list).zip(results) {
            let v = parse_attr_list(&ids, &list)?;
            apply_attrs(a, &v);
        }
        Ok(())
    }

    /// stat one path following final-component symlinks (phased).
    fn stat_one_following(&mut self, index: usize, a: &mut VfAttrs) -> VfResult<()> {
        let path = self
            .server_vf_path(&a.file)
            .map_err(|e| e.with_index(index))?;
        let fh = self
            .resolve_follow(&path)
            .map_err(|e| e.with_index(index))?;
        let ids = request_mask_to_attr_list(&a.masks);
        let list = self
            .nfs
            .getattr(&fh, &ids)
            .map_err(|e| VfError::from_rpc(e, index))?;
        let v = parse_attr_list(&ids, &list)?;
        apply_attrs(a, &v);
        Ok(())
    }

    /// setattr one path following final-component symlinks (phased).
    fn setattr_one_following(&mut self, index: usize, a: &VfAttrs) -> VfResult<()> {
        let path = self
            .server_vf_path(&a.file)
            .map_err(|e| e.with_index(index))?;
        let fh = self
            .resolve_follow(&path)
            .map_err(|e| e.with_index(index))?;
        let mode = if a.masks.contains(AttrMask::MODE) {
            Some(a.mode & 0o7777)
        } else {
            None
        };
        let size = if a.masks.contains(AttrMask::SIZE) {
            Some(a.size)
        } else {
            None
        };
        let atime = a
            .masks
            .contains(AttrMask::ATIME)
            .then_some((a.atime_sec, a.atime_nsec));
        let mtime = a
            .masks
            .contains(AttrMask::MTIME)
            .then_some((a.mtime_sec, a.mtime_nsec));
        self.nfs
            .setattr_values(&fh, mode, size, atime, mtime)
            .map_err(|e| VfError::from_rpc(e, index))
    }

    /// Connect to the NFS server at `host` and resolve the export root.
    pub fn connect(host: &str) -> VfResult<NfsVecFs> {
        Self::connect_with_timeouts(host, None, Duration::from_secs(10), Duration::from_secs(5))
    }

    /// Connect using an explicit NFS minor version (2 enables server COPY).
    pub fn connect_minor(host: &str, minorversion: u32) -> VfResult<NfsVecFs> {
        Self::connect_with_timeouts(
            host,
            Some(minorversion),
            Duration::from_secs(10),
            Duration::from_secs(5),
        )
    }

    /// Connect with explicit setup and per-RPC timeouts.
    pub fn connect_with_timeouts(
        host: &str,
        minorversion: Option<u32>,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> VfResult<NfsVecFs> {
        Self::connect_with_options(
            host,
            NfsConnectOptions {
                minorversion,
                connect_timeout,
                request_timeout,
                ..NfsConnectOptions::default()
            },
        )
    }

    /// Connect using explicit protocol, timeout, and authentication options.
    pub fn connect_with_options(host: &str, options: NfsConnectOptions) -> VfResult<NfsVecFs> {
        let root = path_from_bytes(&normalize_bytes(path_bytes(&options.root)));
        let connection = ConnectionConfig {
            host: host.to_owned(),
            root,
            minorversion: options.minorversion,
            connect_timeout: options.connect_timeout,
            request_timeout: options.request_timeout,
            authentication: options.authentication,
            client_owner: options.client_owner,
            client_verifier: make_verifier(),
        };
        let nfs = Self::connect_client(&connection)?;
        let mut filesystem = Self::from_client(nfs, connection);
        filesystem.recovery_policy = options.recovery_policy;
        filesystem.auto_reconnect = options.auto_reconnect;
        if options.max_compound_bytes != 0 {
            filesystem.set_max_compound_bytes(options.max_compound_bytes);
        }
        // The protocol handshake already resolved the pseudo-root. Only a
        // configured sub-root needs an eager lookup and type check.
        if !filesystem.connection.root.as_os_str().is_empty() {
            let root_attrs = filesystem.stat(Path::new("/"))?;
            if root_attrs.ftype != VfType::Directory {
                return Err(VfError::failure(0, ERR_NOTDIR));
            }
        }
        Ok(filesystem)
    }

    fn connect_client(connection: &ConnectionConfig) -> VfResult<NfsClient> {
        match connection.minorversion {
            Some(version) => NfsClient::connect_minor_with_identity(
                &connection.host,
                version,
                connection.connect_timeout,
                connection.request_timeout,
                &connection.authentication,
                connection.client_owner.as_deref(),
                Some(connection.client_verifier),
            ),
            None => NfsClient::connect_with_identity(
                &connection.host,
                connection.connect_timeout,
                connection.request_timeout,
                &connection.authentication,
                connection.client_owner.as_deref(),
                Some(connection.client_verifier),
            ),
        }
        .map_err(|e| VfError::from_rpc(e, 0))
    }

    fn from_client(nfs: NfsClient, connection: ConnectionConfig) -> NfsVecFs {
        let server_copy_enabled = cfg!(feature = "server-copy") && nfs.minorversion() >= 2;
        // `cwd` is namespace-relative (the `/`-rooted application namespace),
        // not the server path; `server_path` adds `connection.root`.
        let cwd = PathBuf::new();
        NfsVecFs {
            nfs,
            connection,
            recovery_policy: NfsRecoveryPolicy::default(),
            auto_reconnect: true,
            recovery_in_progress: false,
            cwd,
            next_fd: 0,
            open_files: std::collections::HashMap::new(),
            deferred_descriptor_closes: Vec::new(),
            server_copy_enabled,
            server_copy_stats: NfsServerCopyStats::default(),
            merged_mode: MergedIoMode::Full,
            configured_max_compound_bytes: 0,
            observer: None,
            #[cfg(feature = "test-faults")]
            fault_injector: None,
        }
    }

    fn needs_recovery(error: &VfError) -> bool {
        error.is_transport()
            || matches!(
                error.err_no(),
                nfsstat4_NFS4ERR_EXPIRED
                    | nfsstat4_NFS4ERR_GRACE
                    | nfsstat4_NFS4ERR_STALE_CLIENTID
                    | nfsstat4_NFS4ERR_STALE_STATEID
                    | nfsstat4_NFS4ERR_BAD_STATEID
                    | nfsstat4_NFS4ERR_BADSESSION
                    | nfsstat4_NFS4ERR_DEADSESSION
            )
    }

    fn reconnect_once(&mut self) -> VfResult<()> {
        let snapshots: Vec<(i32, ReopenFile, u64)> = self
            .open_files
            .iter()
            .map(|(&fd, open)| {
                open.reopen
                    .clone()
                    .map(|reopen| (fd, reopen, open.cur_offset))
                    .ok_or_else(|| {
                        VfError::transport(
                            None,
                            "cannot recover while an internal temporary descriptor is live",
                        )
                    })
            })
            .collect::<VfResult<_>>()?;
        let nfs = Self::connect_client(&self.connection)?;
        let mut replacement = Self::from_client(nfs, self.connection.clone());
        replacement.recovery_policy = self.recovery_policy;
        replacement.auto_reconnect = self.auto_reconnect;
        replacement.cwd = self.cwd.clone();
        replacement.next_fd = self.next_fd;
        replacement.merged_mode = self.merged_mode;
        replacement.server_copy_stats = self.server_copy_stats;
        replacement.observer = self.observer.clone();
        #[cfg(feature = "test-faults")]
        {
            replacement.fault_injector = self.fault_injector.clone();
            if let Some(injector) = &replacement.fault_injector {
                replacement.nfs.set_fault_injector(injector.clone());
            }
        }
        replacement.configured_max_compound_bytes = self.configured_max_compound_bytes;
        if self.configured_max_compound_bytes != 0 {
            replacement
                .nfs
                .set_max_compound_bytes(self.configured_max_compound_bytes);
        }

        if !snapshots.is_empty() {
            let paths: Vec<&Path> = snapshots
                .iter()
                .map(|(_, open, _)| open.path.as_path())
                .collect();
            let flags: Vec<i32> = snapshots.iter().map(|(_, open, _)| open.flags).collect();
            let modes: Vec<u32> = snapshots.iter().map(|(_, open, _)| open.mode).collect();
            let reopened = VecFs::openv(&mut replacement, &paths, &flags, &modes)?;
            let mut restored = std::collections::HashMap::with_capacity(reopened.len());
            for ((old_fd, _, offset), file) in snapshots.iter().zip(reopened) {
                let new_fd = file.fd().expect("openv returns descriptors");
                let mut open = replacement
                    .open_files
                    .remove(&new_fd)
                    .expect("openv registered descriptor");
                open.cur_offset = *offset;
                restored.insert(*old_fd, open);
            }
            replacement.open_files = restored;
        }

        std::mem::swap(self, &mut replacement);
        // `replacement` now owns the dead session and its obsolete open
        // state. Do not turn successful recovery into several close/destroy
        // RPC timeouts while it is dropped.
        replacement.open_files.clear();
        replacement.nfs.abandon();
        Ok(())
    }

    /// Establish a fresh session and reopen all live path-backed descriptors.
    /// Descriptor numbers and current offsets are preserved. Reopen never
    /// repeats create, exclusive-create, or truncate side effects.
    pub fn reconnect(&mut self) -> VfResult<()> {
        self.notify(NfsEvent::ReconnectStarted);
        let attempts = self.recovery_policy.reconnect_attempts.max(1);
        let mut backoff = self.recovery_policy.initial_backoff;
        let started = std::time::Instant::now();
        let mut last = None;
        for attempt in 0..attempts {
            match self.reconnect_once() {
                Ok(()) => {
                    self.notify(NfsEvent::ReconnectSucceeded);
                    return Ok(());
                }
                Err(error) => last = Some(error),
            }
            let elapsed = started.elapsed();
            if attempt + 1 >= attempts || elapsed >= self.recovery_policy.max_elapsed {
                break;
            }
            if !backoff.is_zero() {
                let remaining = self.recovery_policy.max_elapsed.saturating_sub(elapsed);
                std::thread::sleep(backoff.min(self.recovery_policy.max_backoff).min(remaining));
                backoff = backoff
                    .checked_mul(2)
                    .unwrap_or(self.recovery_policy.max_backoff)
                    .min(self.recovery_policy.max_backoff);
            }
        }
        let error = last.unwrap_or_else(|| VfError::transport(None, "NFS reconnect failed"));
        self.notify(NfsEvent::ReconnectFailed {
            error: error.clone(),
        });
        Err(error)
    }

    fn read_with_recovery<T>(
        &mut self,
        mut operation: impl FnMut(&mut Self) -> VfResult<T>,
    ) -> VfResult<T> {
        debug_assert!(!self.recovery_in_progress);
        self.recovery_in_progress = true;
        let first = operation(self);
        self.recovery_in_progress = false;
        match first {
            Err(error) if self.auto_reconnect && Self::needs_recovery(&error) => {
                self.reconnect()?;
                self.recovery_in_progress = true;
                let retry = operation(self);
                self.recovery_in_progress = false;
                retry
            }
            result => result,
        }
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

    // -- private helpers ----------------------------------------------------

    fn insert_open_file(&mut self, open: OpenFile) -> VfResult<i32> {
        crate::vecfs::insert_fd(&mut self.next_fd, &mut self.open_files, open)
    }

    fn drain_deferred_descriptor_closes(&mut self) -> VfResult<()> {
        if self.deferred_descriptor_closes.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.deferred_descriptor_closes);
        let operations: Vec<crate::client::CloseOp> = pending
            .iter()
            .map(|open| crate::client::CloseOp {
                fh: open.fh.clone(),
                stateid: open.stateid,
            })
            .collect();
        match self.nfs.close_many(&operations) {
            Ok(()) => Ok(()),
            Err(error) => {
                let first_unconfirmed = if error.is_transport() {
                    0
                } else {
                    error.op_index.min(pending.len())
                };
                self.deferred_descriptor_closes
                    .extend(pending.into_iter().skip(first_unconfirmed));
                Err(VfError::from_rpc(error, None))
            }
        }
    }

    /// Resolve a root-relative path to a file handle, following symlinks in
    /// intermediate components (POSIX pathwalk) and, when `follow_final` is
    /// set, the final component too (for `stat`/`open` semantics).
    ///
    /// The fast path is a single deep-resolve compound; when the server
    /// reports `NFS4ERR_SYMLINK` mid-path, resolution falls back to a
    /// component-wise walk that follows each symlink with READLINK, splicing
    /// its target into the remaining path (hop-capped at 40).
    fn resolve_path(&mut self, root_rel: &Path, follow_final: bool) -> VfResult<FileHandle> {
        let mut path = normalize_bytes(path_bytes(root_rel));
        let mut hops = 0usize;
        loop {
            if hops > 40 {
                return Err(VfError::failure(0, nfsstat4_NFS4ERR_IO)); // symlink loop
            }
            match self.nfs.resolve(&path) {
                Ok(fh) => {
                    if !follow_final {
                        return Ok(fh);
                    }
                    let t = self
                        .nfs
                        .getattr(&fh, &[FATTR4_TYPE])
                        .map_err(|e| VfError::from_rpc(e, 0))?;
                    if !type_is_symlink(&t)? {
                        return Ok(fh);
                    }
                    let target = self
                        .nfs
                        .readlink(&fh)
                        .map_err(|e| VfError::from_rpc(e, 0))?;
                    path = self.resolve_target(&path, &target);
                    hops += 1;
                }
                Err(e) if e.status == nfsstat4_NFS4ERR_SYMLINK => {
                    // An intermediate component is a symlink: walk component
                    // by component, following each link we encounter.
                    let comps = components_bytes(&path);
                    if comps.is_empty() {
                        return Err(VfError::from_rpc(e, 0));
                    }
                    let mut cur_fh = self.nfs.root().clone();
                    let mut consumed: Vec<u8> = Vec::new();
                    let mut followed = false;
                    for (i, comp) in comps.iter().enumerate() {
                        let is_last = i + 1 == comps.len();
                        let (child, ftype) = self
                            .nfs
                            .lookup_getattr(&cur_fh, comp)
                            .map_err(|e| VfError::from_rpc(e, 0))?;
                        let full_comp = join_path_bytes(&consumed, comp);
                        if ftype == nfs_ftype4_NF4LNK && (follow_final || !is_last) {
                            let target = self
                                .nfs
                                .readlink(&child)
                                .map_err(|e| VfError::from_rpc(e, 0))?;
                            let mut rest = Vec::new();
                            for (j, r) in comps.iter().enumerate().skip(i + 1) {
                                if j > i + 1 {
                                    rest.push(b'/');
                                }
                                rest.extend_from_slice(r);
                            }
                            let base = self.resolve_target(&full_comp, &target);
                            path = if rest.is_empty() {
                                base
                            } else {
                                join_path_bytes(&base, &rest)
                            };
                            followed = true;
                            break;
                        }
                        cur_fh = child;
                        if is_last {
                            return Ok(cur_fh);
                        }
                        consumed = full_comp;
                    }
                    if !followed {
                        return Err(VfError::from_rpc(e, 0));
                    }
                    hops += 1;
                }
                Err(e) => return Err(VfError::from_rpc(e, 0)),
            }
        }
    }

    /// Map fcntl-style flags to an NFSv4 share access mode.
    fn flags_to_access(flags: i32) -> u32 {
        use libc::{O_RDWR, O_WRONLY};
        if flags & O_RDWR != 0 {
            OPEN4_SHARE_ACCESS_BOTH
        } else if flags & O_WRONLY != 0 {
            OPEN4_SHARE_ACCESS_WRITE
        } else {
            OPEN4_SHARE_ACCESS_READ
        }
    }

    fn open_impl(
        &mut self,
        dir: &Path,
        name: &[u8],
        access: u32,
        create: bool,
        excl: bool,
    ) -> VfResult<(FileHandle, stateid4)> {
        let dirfh = self.resolve_path(dir, true).map_err(|e| e.with_index(0))?;
        let mode = match (create, excl) {
            (false, _) => OpenCreate::NoCreate,
            (true, true) => OpenCreate::Exclusive,
            (true, false) => OpenCreate::Guarded,
        };
        self.nfs
            .open(&dirfh, name, access, mode)
            .map_err(|e| VfError::from_rpc(e, 0))
    }

    /// The merged (single-compound) openv: resolve each parent once, then
    /// OPEN + GETFH per file. UNCHECKED creates carry the mode and the
    /// size=0 `O_TRUNC` in their createattrs, so no existence probe or
    /// separate SETATTR is needed (RFC 8881 §18.16.3).
    fn openv_merged(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<ManyResults<VfFile>> {
        use libc::{O_APPEND, O_CREAT, O_EXCL, O_TRUNC};
        let mut ops = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let create = if flags[i] & O_CREAT != 0 {
                if flags[i] & O_EXCL != 0 {
                    crate::client::OpenCreate::Exclusive
                } else {
                    crate::client::OpenCreate::Unchecked
                }
            } else {
                crate::client::OpenCreate::NoCreate
            };
            ops.push(crate::client::PathOpenOp {
                path: path_bytes(&self.server_path(p)).to_vec(),
                access: Self::flags_to_access(flags[i]),
                create,
                mode: Some(modes[i] & 0o7777),
                truncate: flags[i] & O_TRUNC != 0,
            });
        }
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeDispatch { chunk: 0 })?;
        let mut outcome = self
            .nfs
            .openv_path_compound(&ops)
            .map_err(|e| VfError::from_rpc(e, None))?;
        #[cfg(feature = "test-faults")]
        if let Err(error) = self.inject_open_fault(OpenFaultPoint::AfterReply { chunk: 0 }) {
            let closes: Vec<crate::client::CloseOp> = outcome
                .opened
                .iter_mut()
                .filter_map(Option::take)
                .map(|(fh, stateid)| crate::client::CloseOp { fh, stateid })
                .collect();
            if !closes.is_empty() {
                let _ = self.nfs.close_many_path(&closes);
            }
            return Err(error);
        }
        let failed = outcome.failed;
        let mut results = Vec::with_capacity(paths.len());
        for index in 0..paths.len() {
            let Some((fh, stateid)) = outcome.opened[index].take() else {
                if let Some((failed_index, status)) = failed {
                    debug_assert_eq!(index, failed_index);
                    results.push(Err(VfError::from_rpc(
                        RpcError::op(index, status),
                        Some(index),
                    )));
                }
                break;
            };
            #[cfg(feature = "test-faults")]
            if let Err(error) = self.inject_open_fault(OpenFaultPoint::BeforeRegister { index }) {
                let mut closes = vec![crate::client::CloseOp { fh, stateid }];
                closes.extend(outcome.opened[index + 1..].iter_mut().filter_map(|opened| {
                    opened
                        .take()
                        .map(|(fh, stateid)| crate::client::CloseOp { fh, stateid })
                }));
                let _ = self.nfs.close_many_path(&closes);
                results.push(Err(error));
                break;
            }
            let open = OpenFile {
                fh: fh.clone(),
                stateid,
                cur_offset: 0,
                append: flags[index] & O_APPEND != 0,
                reopen: Some(ReopenFile {
                    path: self.visible_path(&self.server_path(paths[index])),
                    flags: non_destructive_reopen_flags(flags[index]),
                    mode: modes[index],
                }),
            };
            match self.insert_open_file(open) {
                Ok(fd) => {
                    let file = VfFile::from_fd(fd);
                    #[cfg(feature = "test-faults")]
                    if let Err(error) =
                        self.inject_open_fault(OpenFaultPoint::AfterRegister { index })
                    {
                        let _ = self.close(&file);
                        let closes: Vec<crate::client::CloseOp> = outcome.opened[index + 1..]
                            .iter_mut()
                            .filter_map(Option::take)
                            .map(|(fh, stateid)| crate::client::CloseOp { fh, stateid })
                            .collect();
                        if !closes.is_empty() {
                            let _ = self.nfs.close_many_path(&closes);
                        }
                        results.push(Err(error));
                        break;
                    }
                    results.push(Ok(file));
                }
                Err(error) => {
                    let mut closes = vec![crate::client::CloseOp { fh, stateid }];
                    closes.extend(outcome.opened[index + 1..].iter_mut().filter_map(|opened| {
                        opened
                            .take()
                            .map(|(fh, stateid)| crate::client::CloseOp { fh, stateid })
                    }));
                    let _ = self.nfs.close_many_path(&closes);
                    results.push(Err(error));
                    break;
                }
            }
        }
        Ok(ManyResults::new(paths.len(), results))
    }

    /// Open every path-based file in `files` in one batched OPEN compound,
    /// returning, per original index, the temporary descriptor (or `None` for
    /// inputs that already were descriptors). The caller must close them via
    /// [`close_tmp`](Self::close_tmp). Errors are attributed to the original
    /// op index.
    fn open_path_batch(
        &mut self,
        files: &[&VfFile],
        creation: &[bool],
        for_write: bool,
        truncate: &[bool],
    ) -> VfResult<Vec<Option<i32>>> {
        // Resolve each parent directory once per distinct dir, then look up
        // every final component in one tolerant batch (which also reports the
        // type, so symlinks can be followed only when actually present).
        let mut dir_cache: std::collections::HashMap<Vec<u8>, FileHandle> =
            std::collections::HashMap::new();
        let mut lookups: Vec<(usize, FileHandle, Vec<u8>)> = Vec::new();
        for (i, f) in files.iter().enumerate() {
            if f.is_descriptor() {
                continue;
            }
            match f {
                VfFile::Cwd => return Err(VfError::failure(i, ERR_ISDIR)),
                VfFile::Saved => return Err(VfError::failure(i, ERR_NOENT)),
                VfFile::Path { .. } | VfFile::CwdPath(_) => {}
                VfFile::Descriptor(_) => unreachable!(),
                _ => return Err(VfError::unsupported(i)),
            }
            let full = self.server_vf_path(f).map_err(|e| e.with_index(i))?;
            let (dir, name) =
                split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            let dirfh = match dir_cache.get(&dir) {
                Some(fh) => fh.clone(),
                None => {
                    let fh = self
                        .resolve_path(&path_from_bytes(&dir), true)
                        .map_err(|e| e.with_index(i))?;
                    dir_cache.insert(dir.clone(), fh.clone());
                    fh
                }
            };
            lookups.push((i, dirfh, name));
        }
        if lookups.is_empty() {
            return Ok(vec![None; files.len()]);
        }
        let probe: Vec<(FileHandle, Vec<u8>)> = lookups
            .iter()
            .map(|(_, dir, name)| (dir.clone(), name.clone()))
            .collect();
        let results = self
            .nfs
            .lookup_getattr_many(&probe)
            .map_err(|e| VfError::from_rpc(e, None))?;
        let access = if for_write {
            OPEN4_SHARE_ACCESS_BOTH
        } else {
            OPEN4_SHARE_ACCESS_READ
        };
        let mut opens: Vec<(usize, crate::client::OpenOp)> = Vec::new();
        let mut subset: Vec<usize> = Vec::new();
        for ((orig, dirfh, name), r) in lookups.iter().zip(results) {
            match r {
                Ok((_fh, ftype)) if ftype == nfs_ftype4_NF4LNK => {
                    // OPEN cannot target a symlink: follow the chain (creation
                    // follows dangling links and creates the target).
                    let full = &self.server_vf_path(files[*orig])?;
                    let full = self
                        .follow_target_path(full)
                        .map_err(|e| e.with_index(*orig))?;
                    let (dir, name2) = split_path_bytes(path_bytes(&full))
                        .map_err(|_| VfError::failure(*orig, ERR_NOENT))?;
                    let dirfh2 = self
                        .resolve_path(&path_from_bytes(&dir), true)
                        .map_err(|e| e.with_index(*orig))?;
                    let create = match self.nfs.lookup_getattr(&dirfh2, &name2) {
                        Ok(_) => crate::client::OpenCreate::NoCreate,
                        Err(e) if e.status == nfsstat4_NFS4ERR_NOENT => {
                            if !creation[*orig] {
                                return Err(VfError::failure(*orig, ERR_NOENT));
                            }
                            crate::client::OpenCreate::Guarded
                        }
                        Err(e) => return Err(VfError::from_rpc(e, *orig)),
                    };
                    opens.push((
                        *orig,
                        crate::client::OpenOp {
                            dir: dirfh2,
                            name: name2,
                            access,
                            create,
                        },
                    ));
                    subset.push(*orig);
                }
                Ok(_) => {
                    opens.push((
                        *orig,
                        crate::client::OpenOp {
                            dir: dirfh.clone(),
                            name: name.clone(),
                            access,
                            create: crate::client::OpenCreate::NoCreate,
                        },
                    ));
                    subset.push(*orig);
                }
                Err(status) if status == nfsstat4_NFS4ERR_NOENT => {
                    if !creation[*orig] {
                        return Err(VfError::failure(*orig, ERR_NOENT));
                    }
                    opens.push((
                        *orig,
                        crate::client::OpenOp {
                            dir: dirfh.clone(),
                            name: name.clone(),
                            access,
                            create: crate::client::OpenCreate::Guarded,
                        },
                    ));
                    subset.push(*orig);
                }
                Err(status) => return Err(VfError::nfs(*orig, status)),
            }
        }
        if opens.is_empty() {
            return Ok(vec![None; files.len()]);
        }
        let open_ops: Vec<crate::client::OpenOp> = opens
            .iter()
            .map(|(_, op)| crate::client::OpenOp {
                dir: op.dir.clone(),
                name: op.name.clone(),
                access: op.access,
                create: op.create,
            })
            .collect();
        let results = self.nfs.open_many_path(&open_ops).map_err(|e| {
            let e = VfError::from_rpc_indexed(e);
            // open_many's index is relative to the path-only subset.
            e.map_index(|relative| subset.get(relative).copied().unwrap_or(relative))
        })?;
        // Apply O_TRUNC semantics in this phased fallback.
        let mut setattr_ops = Vec::new();
        for (&orig, (fh, _)) in subset.iter().zip(results.iter()) {
            if truncate.get(orig).copied().unwrap_or(false) {
                setattr_ops.push(crate::client::SetattrOp {
                    fh: fh.clone(),
                    mode: None,
                    size: Some(0),
                    atime: None,
                    mtime: None,
                });
            }
        }
        if !setattr_ops.is_empty() {
            self.nfs.setattr_many(&setattr_ops).map_err(|e| {
                let e = VfError::from_rpc_indexed(e);
                e.map_index(|relative| subset.get(relative).copied().unwrap_or(relative))
            })?;
        }
        let mut tmp = vec![None; files.len()];
        for (orig, (fh, stateid)) in subset.iter().zip(results) {
            let fd = self.insert_open_file(OpenFile {
                fh,
                stateid,
                cur_offset: 0,
                append: false,
                reopen: None,
            })?;
            tmp[*orig] = Some(fd);
        }
        Ok(tmp)
    }

    /// Close and forget temporary descriptors opened by
    /// [`open_path_batch`](Self::open_path_batch). Best-effort: never fails
    /// the caller (the close failure would mask the real error).
    fn close_tmp(&mut self, tmp: &[Option<i32>]) {
        let closes: Vec<crate::client::CloseOp> = tmp
            .iter()
            .filter_map(|fd| {
                let fd = (*fd)?;
                self.open_files.remove(&fd).map(|o| crate::client::CloseOp {
                    fh: o.fh,
                    stateid: o.stateid,
                })
            })
            .collect();
        if !closes.is_empty() {
            let _ = self.nfs.close_many_path(&closes);
        }
    }

    /// Batched readv for open (descriptor) ops: one compound per chunk of
    /// files, each carrying `[PUTFH, READ]` for every op.
    fn readv_batch(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        let per = self.nfs.read_per_op_bytes();
        let mut ops = Vec::with_capacity(reads.len());
        let mut offsets = Vec::with_capacity(reads.len());
        let mut owner = Vec::with_capacity(reads.len());
        for (i, op) in reads.iter().enumerate() {
            let off = self
                .resolve_offset(&op.file, op.offset)
                .map_err(|e| e.with_index(i))?;
            let length = u64::try_from(op.length)
                .map_err(|_| VfError::failure(i, libc::EOVERFLOW as u32))?;
            off.checked_add(length)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            let o = self
                .open_files
                .get(&op.file.fd().unwrap())
                .cloned()
                .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
            let mut remaining = op.length;
            let mut chunk_off = 0u64;
            loop {
                let n = remaining.min(per);
                ops.push(crate::client::ReadOp {
                    fh: o.fh.clone(),
                    stateid: o.stateid,
                    offset: off
                        .checked_add(chunk_off)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?,
                    count: n as u32,
                });
                owner.push(i);
                remaining -= n;
                if remaining == 0 {
                    break;
                }
                chunk_off = chunk_off
                    .checked_add(n as u64)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            }
            offsets.push(off);
        }
        // The server validates the summed READ counts of a compound against
        // ca_maxresponsesize. Pack by each chunk's actual count so many small
        // descriptor reads share a compound instead of being pessimistically
        // charged the maximum per-op size.
        let byte_limit = if self.nfs.max_response_bytes > 0 {
            self.nfs.read_compound_bytes().saturating_sub(128)
        } else {
            usize::MAX
        };
        let mut results = Vec::with_capacity(ops.len());
        let mut start = 0;
        while start < ops.len() {
            let mut end = start;
            let mut bytes = 0usize;
            while end < ops.len() {
                let next = ops[end].count as usize;
                if end > start && bytes.saturating_add(next) > byte_limit {
                    break;
                }
                bytes = bytes.saturating_add(next);
                end += 1;
            }
            let r = self.nfs.readv(&ops[start..end]).map_err(|error| {
                let error = VfError::from_rpc_indexed(error);
                remap_descriptor_chunk_error(error, start, &owner)
            })?;
            results.extend(r);
            start = end;
        }
        let mut out = Vec::with_capacity(reads.len());
        let mut ci = 0usize;
        for (i, op) in reads.iter().enumerate() {
            let off = offsets[i];
            let mut data = Vec::new();
            let mut eof = false;
            while ci < owner.len() && owner[ci] == i {
                let (chunk, e) = &results[ci];
                data.extend_from_slice(chunk);
                eof = *e;
                ci += 1;
            }
            let new_offset = off
                .checked_add(data.len() as u64)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            self.advance_offset(&op.file, new_offset);
            out.push(ReadResult {
                file: op.file.clone(),
                offset: off,
                data,
                eof,
            });
        }
        Ok(out)
    }

    /// Resolve a [`VfOffset`] to a concrete file offset.
    fn resolve_offset(&mut self, file: &VfFile, off: VfOffset) -> VfResult<u64> {
        match off {
            VfOffset::At(offset) => Ok(offset),
            VfOffset::Cur => match file.fd() {
                Some(fd) => Ok(self.open_files.get(&fd).map(|o| o.cur_offset).unwrap_or(0)),
                None => Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL)),
            },
            VfOffset::End => {
                let fh = self.resolve_tcfile(file, true)?;
                self.file_size(&fh)
            }
            _ => Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL)),
        }
    }

    /// The offset a write should use: like [`resolve_offset`](Self::resolve_offset),
    /// but descriptors opened with `O_APPEND` always write at the end of the
    /// file (one extra size query per write).
    fn write_offset(&mut self, file: &VfFile, off: VfOffset) -> VfResult<u64> {
        let offset = self.resolve_offset(file, off)?;
        let append_fh = match file.fd() {
            Some(fd) => self
                .open_files
                .get(&fd)
                .filter(|o| o.append)
                .map(|o| o.fh.clone()),
            None => None,
        };
        match append_fh {
            Some(fh) => self.file_size(&fh),
            None => Ok(offset),
        }
    }

    /// Record the new read/write offset of an open (descriptor) file.
    fn advance_offset(&mut self, file: &VfFile, new_offset: u64) {
        if let Some(fd) = file.fd()
            && let Some(o) = self.open_files.get_mut(&fd)
        {
            o.cur_offset = new_offset;
        }
    }

    /// Batched writev for open (descriptor) ops.
    fn writev_batch(&mut self, writes: &[WriteOp]) -> VfResult<Vec<WriteResult>> {
        let per = self.nfs.per_op_bytes();
        let mut ops = Vec::with_capacity(writes.len());
        let mut offsets = Vec::with_capacity(writes.len());
        let mut owner = Vec::with_capacity(writes.len());
        for (i, op) in writes.iter().enumerate() {
            let off = self
                .write_offset(&op.file, op.offset)
                .map_err(|e| e.with_index(i))?;
            let length = u64::try_from(op.data.len())
                .map_err(|_| VfError::failure(i, libc::EOVERFLOW as u32))?;
            off.checked_add(length)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            let o = self
                .open_files
                .get(&op.file.fd().unwrap())
                .cloned()
                .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
            let mut remaining = op.data.len();
            let mut chunk_off = 0u64;
            loop {
                let n = remaining.min(per);
                ops.push(crate::client::WriteOp {
                    fh: o.fh.clone(),
                    stateid: o.stateid,
                    offset: off
                        .checked_add(chunk_off)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?,
                    data: op.data[chunk_off as usize..chunk_off as usize + n].to_vec(),
                });
                owner.push(i);
                remaining -= n;
                if remaining == 0 {
                    break;
                }
                chunk_off = chunk_off
                    .checked_add(n as u64)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            }
            offsets.push(off);
        }
        // Keep each compound's request under ca_maxrequestsize. Pack by the
        // actual chunk lengths so small writes retain vectorization.
        let byte_limit = if self.nfs.max_compound_bytes > 0 {
            self.nfs.max_compound_bytes.saturating_sub(128)
        } else {
            usize::MAX
        };
        let mut results = Vec::with_capacity(ops.len());
        let mut start = 0;
        #[cfg(feature = "test-faults")]
        let mut chunk_index = 0usize;
        while start < ops.len() {
            let mut end = start;
            let mut bytes = 0usize;
            while end < ops.len() {
                let next = ops[end].data.len();
                if end > start && bytes.saturating_add(next) > byte_limit {
                    break;
                }
                bytes = bytes.saturating_add(next);
                end += 1;
            }
            let r = self.nfs.writev(&ops[start..end]).map_err(|error| {
                let error = VfError::from_rpc_indexed(error);
                remap_descriptor_chunk_error(error, start, &owner)
            })?;
            for (local_index, (written, _)) in r.iter().enumerate() {
                let wire_index = start + local_index;
                let request_index = owner[wire_index];
                let new_offset = ops[wire_index]
                    .offset
                    .checked_add(*written as u64)
                    .ok_or_else(|| VfError::failure(request_index, libc::EOVERFLOW as u32))?;
                self.advance_offset(&writes[request_index].file, new_offset);
            }
            results.extend(r);
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::AfterWriteChunk { chunk: chunk_index })?;
            start = end;
            #[cfg(feature = "test-faults")]
            {
                chunk_index += 1;
            }
        }
        let mut out = Vec::with_capacity(writes.len());
        let mut ci = 0usize;
        for (i, op) in writes.iter().enumerate() {
            let off = offsets[i];
            let mut written = 0u64;
            let mut stable = true;
            while ci < owner.len() && owner[ci] == i {
                let (n, committed) = &results[ci];
                written = written
                    .checked_add(*n as u64)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                stable = stable && *committed == stable_how4_FILE_SYNC4;
                ci += 1;
            }
            let new_offset = off
                .checked_add(written)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            self.advance_offset(&op.file, new_offset);
            out.push(WriteResult {
                file: op.file.clone(),
                offset: off,
                written: written as usize,
                stable,
            });
        }
        Ok(out)
    }

    /// The size in bytes of `fh`.
    fn file_size(&mut self, fh: &FileHandle) -> VfResult<u64> {
        let list = self
            .nfs
            .getattr(fh, &[FATTR4_SIZE])
            .map_err(|e| VfError::from_rpc(e, 0))?;
        let mut off = 0;
        read_u64(&list, &mut off)
    }

    fn resolve_tcfile(&mut self, f: &VfFile, follow: bool) -> VfResult<FileHandle> {
        match f {
            VfFile::Descriptor(fd) => self
                .open_files
                .get(fd)
                .map(|o| o.fh.clone())
                .ok_or_else(|| VfError::failure(0, ERR_EBADF)),
            VfFile::Path { .. } | VfFile::Cwd | VfFile::CwdPath(_) => {
                let path = self.server_vf_path(f)?;
                if follow {
                    self.resolve_follow(&path)
                } else {
                    self.resolve_path(&path, false)
                }
            }
            VfFile::Saved => Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL)),
            _ => Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL)),
        }
    }

    /// Resolve a symlink target against the link's parent directory (POSIX
    /// semantics), returning a normalized root-relative path. Absolute
    /// targets resolve from the configured application namespace root.
    fn resolve_target(&self, link_path: &[u8], target: &[u8]) -> Vec<u8> {
        let namespace_root = path_bytes(&self.connection.root);
        let link_relative = link_path
            .strip_prefix(namespace_root)
            .map(|path| path.strip_prefix(b"/").unwrap_or(path))
            .unwrap_or(link_path);
        let relative = if target.first() == Some(&b'/') {
            normalize_bytes(&target[1..])
        } else {
            let mut combined = Vec::new();
            if let Some(idx) = link_relative.iter().rposition(|&b| b == b'/') {
                combined.extend_from_slice(&link_relative[..=idx]);
            };
            combined.extend_from_slice(target);
            normalize_bytes(&combined)
        };
        path_bytes(&self.connection.root.join(path_from_bytes(&relative))).to_vec()
    }

    fn follow_target_path(&mut self, root_rel: &Path) -> VfResult<PathBuf> {
        let mut current = normalize_bytes(path_bytes(root_rel));
        let mut hops = 0usize;
        loop {
            let namespace_root = path_bytes(&self.connection.root);
            let visible = current
                .strip_prefix(namespace_root)
                .map(|path| path.strip_prefix(b"/").unwrap_or(path))
                .unwrap_or(&current);
            let mut abs = b"/".to_vec();
            abs.extend_from_slice(visible);
            let abs_path = path_from_bytes(&abs);
            let st = match self.lstat(&abs_path) {
                Ok(s) => s,
                Err(e) if e.err_no() == ERR_NOENT => return Ok(path_from_bytes(&current)),
                Err(e) => return Err(e),
            };
            if st.ftype != VfType::Symlink {
                return Ok(path_from_bytes(&current));
            }
            if hops >= 40 {
                return Err(VfError::failure(0, nfsstat4_NFS4ERR_IO)); // symlink loop
            }
            let target = self.readlink(&abs_path)?;
            current = self.resolve_target(&current, &target);
            hops += 1;
        }
    }

    fn resolve_follow(&mut self, path: &Path) -> VfResult<FileHandle> {
        self.resolve_path(path, true)
    }

    fn listdir_rec(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
        out: &mut Vec<VfAttrs>,
    ) -> VfRes {
        let reached_limit = |out: &Vec<VfAttrs>| max_count != 0 && out.len() >= max_count;
        let ids = request_mask_to_attr_list(&masks);
        let mut pending = vec![dir.to_path_buf()];
        while let Some(current) = pending.pop() {
            if reached_limit(out) {
                break;
            }
            // A directory argument may itself be a symlink to a directory.
            let dirfh = self.resolve_path(&self.server_path(&current), true)?;
            let mut children = Vec::new();
            let mut cookie = 0u64;
            loop {
                let entries = self
                    .nfs
                    .readdir(&dirfh, cookie, &ids)
                    .map_err(|e| VfError::from_rpc(e, 0))?;
                if entries.is_empty() {
                    break;
                }
                for entry in &entries {
                    if reached_limit(out) {
                        return Ok(());
                    }
                    let path = current.join(path_from_bytes(&entry.name));
                    let mut attrs = VfAttrs {
                        file: VfFile::from_os_path(&path),
                        masks,
                        ..VfAttrs::default()
                    };
                    let values = parse_attr_list(&ids, &entry.attrs)?;
                    apply_attrs(&mut attrs, &values);
                    if recursive && attrs.ftype == VfType::Directory {
                        children.push(path);
                    }
                    out.push(attrs);
                }
                cookie = entries.last().expect("non-empty READDIR page").cookie;
                if cookie == 0 {
                    break;
                }
            }
            for child in children.into_iter().rev() {
                pending.push(child);
            }
        }
        Ok(())
    }

    fn rm_one(&mut self, path: &Path, recursive: bool) -> VfResult<()> {
        let mut pending = vec![(path.to_path_buf(), false)];
        while let Some((current, visited)) = pending.pop() {
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::BeforeRemoveType { index: 0 })?;
            let file_type = self.file_type(&current)?;
            if file_type == VfType::Directory && recursive && !visited {
                pending.push((current.clone(), true));
                let entries = self.listdir(&current, AttrMask::default(), usize::MAX, false)?;
                for entry in entries.into_iter().rev() {
                    let child = entry
                        .file
                        .path()
                        .ok_or_else(|| VfError::failure(0, ERR_INVAL))?
                        .to_path_buf();
                    pending.push((child, false));
                }
            } else {
                self.unlink(&current)?;
            }
        }
        Ok(())
    }

    fn copy_extent(
        &mut self,
        src_root_rel: &Path,
        dst_root_rel: &Path,
        p: &ExtentPair,
    ) -> VfResult<()> {
        let (sdir, sname) = split_path_bytes(path_bytes(src_root_rel))
            .map_err(|_| VfError::failure(0, ERR_NOENT))?;
        let (ddir, dname) = split_path_bytes(path_bytes(dst_root_rel))
            .map_err(|_| VfError::failure(0, ERR_NOENT))?;
        let sdirfh = self
            .resolve_path(&path_from_bytes(&sdir), true)
            .map_err(|e| e.with_index(0))?;
        let ddirfh = self
            .resolve_path(&path_from_bytes(&ddir), true)
            .map_err(|e| e.with_index(0))?;
        let (sfh, ssid) = self
            .nfs
            .open_path(
                &sdirfh,
                &sname,
                OPEN4_SHARE_ACCESS_READ,
                crate::client::OpenCreate::NoCreate,
            )
            .map_err(|e| VfError::from_rpc(e, 0))?;
        // Kernel nfsd rejects CREATE_GUARDED on an existing file, so open the
        // destination without create and fall back to create only on NOENT.
        let (dfh, dsid) = match self
            .nfs
            .open_path(
                &ddirfh,
                &dname,
                OPEN4_SHARE_ACCESS_WRITE,
                crate::client::OpenCreate::NoCreate,
            )
            .map_err(|e| VfError::from_rpc(e, 0))
        {
            Ok(x) => x,
            Err(e) if e.err_no() == ERR_NOENT => self
                .nfs
                .open_path(
                    &ddirfh,
                    &dname,
                    OPEN4_SHARE_ACCESS_WRITE,
                    crate::client::OpenCreate::Guarded,
                )
                .map_err(|e| VfError::from_rpc(e, 0))?,
            Err(e) => {
                let _ = self.nfs.close_path(&sfh, &ssid);
                return Err(e);
            }
        };

        let mut so = p.src_offset;
        let mut doff = p.dst_offset;
        let mut copied: u64 = 0;
        let result = loop {
            if let Some(length) = p.length
                && copied >= length
            {
                break Ok(());
            }
            let remaining = p.length.map_or(u64::MAX, |l| l - copied);
            let chunk_len = remaining.min(1 << 20) as u32;
            let chunk = match self.nfs.read(&sfh, &ssid, so, chunk_len) {
                Ok((c, _)) => c,
                Err(e) => break Err(VfError::from_rpc(e, 0)),
            };
            if chunk.is_empty() {
                break Ok(()); // EOF
            }
            let n = match self.nfs.write(&dfh, &dsid, doff, &chunk) {
                Ok((n, _)) => n as u64,
                Err(e) => break Err(VfError::from_rpc(e, 0)),
            };
            so = so
                .checked_add(n)
                .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?;
            doff = doff
                .checked_add(n)
                .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?;
            copied = copied
                .checked_add(n)
                .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?;
        };
        let result = if result.is_ok() {
            // Truncate any stale tail beyond what was copied (cp semantics).
            self.nfs
                .setattr(&dfh, None, Some(doff))
                .map_err(|e| VfError::from_rpc(e, 0))
        } else {
            result
        };
        let source_close = self
            .nfs
            .close_path(&sfh, &ssid)
            .map_err(|e| VfError::from_rpc(e, 0));
        let destination_close = self
            .nfs
            .close_path(&dfh, &dsid)
            .map_err(|e| VfError::from_rpc(e, 0));
        result.and(source_close).and(destination_close)
    }

    fn copy_extents_server_side(&mut self, pairs: &[ExtentPair]) -> VfRes {
        let src_files: Vec<VfFile> = pairs
            .iter()
            .map(|p| VfFile::from_os_path(&p.src_path))
            .collect();
        let dst_files: Vec<VfFile> = pairs
            .iter()
            .map(|p| VfFile::from_os_path(&p.dst_path))
            .collect();
        let src_refs: Vec<&VfFile> = src_files.iter().collect();
        let dst_refs: Vec<&VfFile> = dst_files.iter().collect();
        let no_create = vec![false; pairs.len()];
        let create = vec![true; pairs.len()];
        let no_truncate = vec![false; pairs.len()];

        let src_tmp = self.open_path_batch(&src_refs, &no_create, false, &no_truncate)?;
        let dst_tmp = match self.open_path_batch(&dst_refs, &create, true, &no_truncate) {
            Ok(tmp) => tmp,
            Err(e) => {
                self.close_tmp(&src_tmp);
                return Err(e);
            }
        };

        let result = (|| {
            let explicit: Vec<usize> = pairs
                .iter()
                .enumerate()
                .filter_map(|(i, pair)| pair.length.is_some_and(|length| length > 0).then_some(i))
                .collect();
            let mut source_attrs: Vec<VfAttrs> = explicit
                .iter()
                .map(|&i| VfAttrs {
                    file: VfFile::from_fd(src_tmp[i].expect("path source was opened")),
                    masks: AttrMask::SIZE,
                    ..VfAttrs::default()
                })
                .collect();
            if !source_attrs.is_empty() {
                self.getattrsv(&mut source_attrs)?;
            }
            let mut effective_lengths: Vec<Option<u64>> =
                pairs.iter().map(|pair| pair.length).collect();
            for (&i, attrs) in explicit.iter().zip(&source_attrs) {
                effective_lengths[i] = pairs[i]
                    .length
                    .map(|length| length.min(attrs.size.saturating_sub(pairs[i].src_offset)));
            }
            let mut copies = Vec::with_capacity(pairs.len());
            for (i, p) in pairs.iter().enumerate() {
                let src = self
                    .open_files
                    .get(&src_tmp[i].expect("path source was opened"))
                    .expect("temporary source descriptor");
                let dst = self
                    .open_files
                    .get(&dst_tmp[i].expect("path destination was opened"))
                    .expect("temporary destination descriptor");
                copies.push(crate::client::CopyOp {
                    src_fh: src.fh.clone(),
                    src_stateid: src.stateid,
                    dst_fh: dst.fh.clone(),
                    dst_stateid: dst.stateid,
                    src_offset: p.src_offset,
                    dst_offset: p.dst_offset,
                    count: effective_lengths[i].unwrap_or(0),
                });
            }
            let mut totals = vec![0u64; copies.len()];
            // NFSv4.2 uses count=0 to mean "through EOF", while the VFSI API
            // uses Some(0) for an explicit zero-byte copy. Do not put those
            // operations on the wire; the SETATTR phase below still applies
            // the destination-size semantics.
            let mut pending: Vec<usize> = pairs
                .iter()
                .enumerate()
                .filter_map(|(i, _)| (effective_lengths[i] != Some(0)).then_some(i))
                .collect();
            while !pending.is_empty() {
                let active: Vec<crate::client::CopyOp> = pending
                    .iter()
                    .map(|&i| crate::client::CopyOp {
                        src_fh: copies[i].src_fh.clone(),
                        src_stateid: copies[i].src_stateid,
                        dst_fh: copies[i].dst_fh.clone(),
                        dst_stateid: copies[i].dst_stateid,
                        src_offset: copies[i].src_offset,
                        dst_offset: copies[i].dst_offset,
                        count: effective_lengths[i]
                            .map(|length| length.saturating_sub(totals[i]))
                            .unwrap_or(0),
                    })
                    .collect();
                self.server_copy_stats.requests += 1;
                let counts = self.nfs.copy_many(&active).map_err(|e| {
                    let original = pending.get(e.op_index).copied().unwrap_or(0);
                    VfError::from_rpc_indexed(e.with_op_index(original))
                })?;
                self.server_copy_stats.operations += counts.len() as u64;
                let mut next = Vec::new();
                for (&i, n) in pending.iter().zip(counts) {
                    totals[i] = totals[i]
                        .checked_add(n)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                    copies[i].src_offset = copies[i]
                        .src_offset
                        .checked_add(n)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                    copies[i].dst_offset = copies[i]
                        .dst_offset
                        .checked_add(n)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                    // A zero-byte continuation means EOF. For an explicit
                    // count this matches dupv's short-at-EOF behavior; for a
                    // count of zero it confirms that a possibly partial COPY
                    // has reached EOF.
                    if n != 0
                        && effective_lengths[i]
                            .map(|length| totals[i] < length)
                            .unwrap_or(true)
                    {
                        next.push(i);
                    }
                }
                pending = next;
            }
            let mut attrs = Vec::with_capacity(pairs.len());
            for (i, (&n, p)) in totals.iter().zip(pairs).enumerate() {
                let size = p
                    .dst_offset
                    .checked_add(n)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                attrs.push(crate::client::SetattrOp {
                    fh: copies[i].dst_fh.clone(),
                    mode: None,
                    size: Some(size),
                    atime: None,
                    mtime: None,
                });
            }
            self.nfs
                .setattr_many(&attrs)
                .map_err(VfError::from_rpc_indexed)
        })();
        self.close_tmp(&src_tmp);
        self.close_tmp(&dst_tmp);
        result
    }
}

// ---------------------------------------------------------------------------
// Merged path I/O plumbing (single-compound readv/writev)
// ---------------------------------------------------------------------------

impl NfsVecFs {
    /// The legacy phased path-based readv (resolve + probe + open + read +
    /// close compounds).
    fn readv_path_fallback(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        if reads.is_empty() {
            return Ok(Vec::new());
        }
        let mut tmp: Vec<Option<i32>> = vec![None; reads.len()];
        let mut files = Vec::with_capacity(reads.len());
        let mut needs_open = false;
        for (i, r) in reads.iter().enumerate() {
            files.push(&r.file);
            if !r.file.is_descriptor() {
                needs_open = true;
                if r.offset == VfOffset::Cur {
                    // "current position" only exists for open descriptors.
                    return Err(VfError::failure(i, ERR_INVAL));
                }
            }
        }
        if needs_open {
            tmp = self.open_path_batch(
                &files,
                &vec![false; reads.len()],
                false,
                &vec![false; reads.len()],
            )?;
        }
        let remapped: Vec<ReadOp> = reads
            .iter()
            .enumerate()
            .map(|(i, r)| ReadOp {
                file: match tmp[i] {
                    Some(fd) => VfFile::from_fd(fd),
                    None => r.file.clone(),
                },
                offset: r.offset,
                length: r.length,
            })
            .collect();
        let result = self.readv_batch(&remapped);
        self.close_tmp(&tmp);
        let mut out = result?;
        for (i, r) in out.iter_mut().enumerate() {
            r.file = reads[i].file.clone();
        }
        Ok(out)
    }

    /// One open+read compound, then a separate close compound with the real
    /// stateids (portable fallback).
    fn readv_path_openwrite(
        &mut self,
        reads: &[ReadOp],
        path_ops: &[crate::client::PathReadOp],
        offsets: &[u64],
    ) -> VfResult<Vec<ReadResult>> {
        match self.nfs.readv_path_compound(path_ops, false) {
            Ok(outcome) => {
                if let Some((i, _st)) = outcome.failed {
                    self.close_path_opens(&outcome.opened);
                    // Prefix [0..i) succeeded; resume [i..] via the phased
                    // path, re-attributing its error to the original index.
                    let mut out = self.assemble_reads(reads, offsets, &outcome.data, &outcome.eof);
                    match self.readv_path_fallback(&reads[i..]) {
                        Ok(suffix) => {
                            for (k, r) in suffix.into_iter().enumerate() {
                                out[i + k] = r;
                            }
                            return Ok(out);
                        }
                        Err(e) => {
                            return Err(e.map_index(|rel| i + rel));
                        }
                    }
                }
                self.close_path_opens(&outcome.opened);
                Ok(self.assemble_reads(reads, offsets, &outcome.data, &outcome.eof))
            }
            Err(error) if error.is_transport() => Err(VfError::from_rpc(error, None)),
            Err(_) => self.readv_path_fallback(reads),
        }
    }

    /// One compound for the whole batch, including the special-stateid
    /// CLOSE. Downgrades to the open+close form if the server rejects that
    /// CLOSE.
    fn readv_path_full(
        &mut self,
        reads: &[ReadOp],
        path_ops: &[crate::client::PathReadOp],
        offsets: &[u64],
    ) -> VfResult<Vec<ReadResult>> {
        match self.nfs.readv_path_compound(path_ops, true) {
            Ok(outcome) => {
                if let Some((i, st)) = outcome.failed {
                    if Self::is_stateid_error(st) {
                        // The special stateid itself is rejected: disable the
                        // merged path entirely.
                        self.merged_mode = MergedIoMode::Off;
                    }
                    self.close_path_opens(&outcome.opened);
                    let mut out = self.assemble_reads(reads, offsets, &outcome.data, &outcome.eof);
                    match self.readv_path_fallback(&reads[i..]) {
                        Ok(suffix) => {
                            for (k, r) in suffix.into_iter().enumerate() {
                                out[i + k] = r;
                            }
                            return Ok(out);
                        }
                        Err(e) => {
                            return Err(e.map_index(|rel| i + rel));
                        }
                    }
                }
                if let Some(_st) = outcome.close_failed {
                    self.merged_mode = MergedIoMode::OpenWrite;
                    return self.readv_path_openwrite(reads, path_ops, offsets);
                }
                Ok(self.assemble_reads(reads, offsets, &outcome.data, &outcome.eof))
            }
            Err(error) if error.is_transport() => Err(VfError::from_rpc(error, None)),
            Err(_) => self.readv_path_fallback(reads),
        }
    }

    fn assemble_reads(
        &self,
        reads: &[ReadOp],
        offsets: &[u64],
        data: &[Option<Vec<u8>>],
        eof: &[Option<bool>],
    ) -> Vec<ReadResult> {
        reads
            .iter()
            .enumerate()
            .map(|(i, r)| ReadResult {
                file: r.file.clone(),
                offset: offsets[i],
                data: data[i].clone().unwrap_or_default(),
                eof: eof[i].unwrap_or(false),
            })
            .collect()
    }

    /// The legacy phased path-based writev.
    fn writev_path_fallback(&mut self, writes: &[WriteOp]) -> VfResult<Vec<WriteResult>> {
        if writes.is_empty() {
            return Ok(Vec::new());
        }
        let mut tmp: Vec<Option<i32>> = vec![None; writes.len()];
        let mut files = Vec::with_capacity(writes.len());
        let mut creation = Vec::with_capacity(writes.len());
        let mut needs_open = false;
        for (i, w) in writes.iter().enumerate() {
            files.push(&w.file);
            creation.push(w.creation);
            if !w.file.is_descriptor() {
                needs_open = true;
                if w.offset == VfOffset::Cur {
                    return Err(VfError::failure(i, ERR_INVAL));
                }
            }
        }
        if needs_open {
            let truncation: Vec<bool> = writes.iter().map(|w| w.truncate).collect();
            tmp = self.open_path_batch(&files, &creation, true, &truncation)?;
        }
        let remapped: Vec<WriteOp> = writes
            .iter()
            .enumerate()
            .map(|(i, w)| WriteOp {
                file: match tmp[i] {
                    Some(fd) => VfFile::from_fd(fd),
                    None => w.file.clone(),
                },
                offset: w.offset,
                data: w.data.clone(),
                creation: false,
                truncate: false,
            })
            .collect();
        let result = self.writev_batch(&remapped);
        self.close_tmp(&tmp);
        let mut out = result?;
        for (i, r) in out.iter_mut().enumerate() {
            r.file = writes[i].file.clone();
        }
        Ok(out)
    }

    fn writev_path_openwrite(
        &mut self,
        writes: &[WriteOp],
        path_ops: &[crate::client::PathWriteOp],
        offsets: &[u64],
    ) -> VfResult<Vec<WriteResult>> {
        match self.nfs.writev_path_compound(path_ops, false) {
            Ok(outcome) => {
                if let Some((i, _st)) = outcome.failed {
                    self.close_path_opens(&outcome.opened);
                    let mut out =
                        self.assemble_writes(writes, offsets, &outcome.counts, &outcome.committed);
                    match self.writev_path_fallback(&writes[i..]) {
                        Ok(suffix) => {
                            for (k, r) in suffix.into_iter().enumerate() {
                                out[i + k] = r;
                            }
                            return Ok(out);
                        }
                        Err(e) => {
                            return Err(e.map_index(|rel| i + rel));
                        }
                    }
                }
                self.close_path_opens(&outcome.opened);
                Ok(self.assemble_writes(writes, offsets, &outcome.counts, &outcome.committed))
            }
            Err(error) if error.is_transport() => Err(VfError::from_rpc(error, None)),
            Err(_) => self.writev_path_fallback(writes),
        }
    }

    fn writev_path_full(
        &mut self,
        writes: &[WriteOp],
        path_ops: &[crate::client::PathWriteOp],
        offsets: &[u64],
    ) -> VfResult<Vec<WriteResult>> {
        match self.nfs.writev_path_compound(path_ops, true) {
            Ok(outcome) => {
                if let Some((i, st)) = outcome.failed {
                    if Self::is_stateid_error(st) {
                        self.merged_mode = MergedIoMode::Off;
                    }
                    self.close_path_opens(&outcome.opened);
                    let mut out =
                        self.assemble_writes(writes, offsets, &outcome.counts, &outcome.committed);
                    match self.writev_path_fallback(&writes[i..]) {
                        Ok(suffix) => {
                            for (k, r) in suffix.into_iter().enumerate() {
                                out[i + k] = r;
                            }
                            return Ok(out);
                        }
                        Err(e) => {
                            return Err(e.map_index(|rel| i + rel));
                        }
                    }
                }
                if let Some(_st) = outcome.close_failed {
                    self.merged_mode = MergedIoMode::OpenWrite;
                    return self.writev_path_openwrite(writes, path_ops, offsets);
                }
                Ok(self.assemble_writes(writes, offsets, &outcome.counts, &outcome.committed))
            }
            Err(error) if error.is_transport() => Err(VfError::from_rpc(error, None)),
            Err(_) => self.writev_path_fallback(writes),
        }
    }

    fn assemble_writes(
        &self,
        writes: &[WriteOp],
        offsets: &[u64],
        counts: &[Option<u32>],
        committed: &[Option<u32>],
    ) -> Vec<WriteResult> {
        writes
            .iter()
            .enumerate()
            .map(|(i, w)| WriteResult {
                file: w.file.clone(),
                offset: offsets[i],
                written: counts[i].unwrap_or(0) as usize,
                stable: committed[i].unwrap_or(0) == stable_how4_FILE_SYNC4,
            })
            .collect()
    }

    /// Best-effort close of stateids opened by a merged path compound.
    fn close_path_opens(&mut self, opens: &[(crate::client::FileHandle, stateid4)]) {
        if opens.is_empty() {
            return;
        }
        let ops: Vec<crate::client::CloseOp> = opens
            .iter()
            .map(|(fh, sid)| crate::client::CloseOp {
                fh: fh.clone(),
                stateid: *sid,
            })
            .collect();
        let _ = self.nfs.close_many_path(&ops);
    }

    /// The legacy phased renamev (cached parent resolution + rename_many).
    fn renamev_phased(&mut self, pairs: &[(VfFile, VfFile)]) -> VfRes {
        let mut src_cache: std::collections::HashMap<Vec<u8>, FileHandle> =
            std::collections::HashMap::new();
        let mut dst_cache: std::collections::HashMap<Vec<u8>, FileHandle> =
            std::collections::HashMap::new();
        let mut ops = Vec::with_capacity(pairs.len());
        for (i, (src, dst)) in pairs.iter().enumerate() {
            let s = self.server_vf_path(src).map_err(|e| e.with_index(i))?;
            let d = self.server_vf_path(dst).map_err(|e| e.with_index(i))?;
            let (sdir, sname) =
                split_path_bytes(path_bytes(&s)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            let (ddir, dname) =
                split_path_bytes(path_bytes(&d)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            let sdirfh = match src_cache.get(&sdir) {
                Some(fh) => fh.clone(),
                None => {
                    let fh = self
                        .resolve_path(&path_from_bytes(&sdir), true)
                        .map_err(|e| e.with_index(i))?;
                    src_cache.insert(sdir.clone(), fh.clone());
                    fh
                }
            };
            let ddirfh = match dst_cache.get(&ddir) {
                Some(fh) => fh.clone(),
                None => {
                    let fh = self
                        .resolve_path(&path_from_bytes(&ddir), true)
                        .map_err(|e| e.with_index(i))?;
                    dst_cache.insert(ddir.clone(), fh.clone());
                    fh
                }
            };
            ops.push(crate::client::RenameOp {
                srcdir: sdirfh,
                oldname: sname,
                dstdir: ddirfh,
                newname: dname,
            });
        }
        self.nfs
            .rename_many(&ops)
            .map_err(VfError::from_rpc_indexed)
    }

    /// The legacy phased removev (grouped per parent + remove_many).
    fn removev_phased(&mut self, files: &[VfFile]) -> VfRes {
        use std::collections::BTreeMap;
        // Group by parent directory to batch REMOVEs.
        let mut groups: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
        for (i, f) in files.iter().enumerate() {
            let path = self.server_vf_path(f).map_err(|e| e.with_index(i))?;
            let (dir, name) =
                split_path_bytes(path_bytes(&path)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            groups.entry(dir).or_default().push(name);
        }
        for (dir, names) in &groups {
            let dirfh = self
                .resolve_path(&path_from_bytes(dir), true)
                .map_err(|e| e.with_index(0))?;
            self.nfs
                .remove_many(&dirfh, names)
                .map_err(VfError::from_rpc_indexed)?;
        }
        Ok(())
    }

    /// Apply the requested modes of `dirs` in one batched resolve + SETATTR.
    /// Used by mkdirv after creation (NFSv4 CREATE cannot carry mode attrs).
    fn apply_dir_modes(&mut self, dirs: &[VfAttrs]) -> VfRes {
        let mut paths: Vec<PathBuf> = Vec::with_capacity(dirs.len());
        let mut indices = Vec::with_capacity(dirs.len());
        for (i, a) in dirs.iter().enumerate() {
            if a.masks.contains(AttrMask::MODE) {
                let path = match self.server_vf_path(&a.file) {
                    Ok(p) => p,
                    Err(e) => return Err(e.with_index(i)),
                };
                paths.push(self.visible_path(&path));
                indices.push(i);
            }
        }
        if paths.is_empty() {
            return Ok(());
        }
        let files: Vec<VfFile> = paths.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let resolved = match self.resolve_many_tcfile(&refs, true) {
            Ok(r) => r,
            Err(e) => {
                return Err(e.map_index(|rel| indices.get(rel).copied().unwrap_or(rel)));
            }
        };
        let mut setattrs = Vec::with_capacity(dirs.len());
        for (k, r) in resolved.iter().enumerate() {
            match r {
                Ok((fh, _)) => setattrs.push(crate::client::SetattrOp {
                    fh: fh.clone(),
                    mode: Some(dirs[indices[k]].mode & 0o7777),
                    size: None,
                    atime: None,
                    mtime: None,
                }),
                Err(status) => return Err(VfError::nfs(indices[k], *status)),
            }
        }
        if !setattrs.is_empty() {
            self.nfs.setattr_many(&setattrs).map_err(|e| {
                let rel = e.op_index;
                let orig = indices.get(rel).copied().unwrap_or(0);
                VfError::from_rpc_indexed(e.with_op_index(orig))
            })?;
        }
        Ok(())
    }

    /// Whether an NFS status indicates the special stateid was rejected
    /// (rather than a per-file failure), so the merged path must be disabled.
    fn is_stateid_error(status: u32) -> bool {
        matches!(
            status,
            nfsstat4_NFS4ERR_BAD_STATEID
                | nfsstat4_NFS4ERR_OLD_STATEID
                | nfsstat4_NFS4ERR_STALE_STATEID
                | nfsstat4_NFS4ERR_BAD_SEQID
                | nfsstat4_NFS4ERR_NOTSUPP
        )
    }
}

impl VecFs for NfsVecFs {
    fn nfs_minorversion(&self) -> Option<u32> {
        Some(self.minorversion())
    }

    fn capabilities(&self) -> u64 {
        VF_CAP_UNIX_SEMANTICS
            | if self.server_copy_enabled() {
                VF_CAP_SERVER_COPY
            } else {
                0
            }
    }

    /// Namespace-relative path (no leading `/`), per the [`VecFs::abs_path`]
    /// contract. The export root (`connection.root`) is not included; use
    /// `NfsVecFs::server_path` for the path sent to the NFS server.
    fn abs_path(&self, path: &Path) -> PathBuf {
        namespace_path(&self.cwd, path)
    }

    fn open_by_path(
        &mut self,
        base: VfPathBase,
        pathname: &Path,
        flags: i32,
        mode: u32,
    ) -> VfResult<VfFile> {
        use libc::{O_APPEND, O_CREAT, O_EXCL, O_TRUNC};
        let full = match base {
            VfPathBase::Abs => self.server_path(&Path::new("/").join(pathname)),
            VfPathBase::Cwd => self.server_path(pathname),
        };
        let full = path_from_bytes(&normalize_bytes(path_bytes(&full)));
        // Follow a final symlink chain so O_CREAT creates the target
        // (POSIX semantics); OPEN cannot target a symlink directly.
        let full = self.follow_target_path(&full)?;
        let (dir, name) =
            split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(0, ERR_NOENT))?;
        let access = Self::flags_to_access(flags);
        let create = flags & O_CREAT != 0;
        let excl = flags & O_EXCL != 0;
        // The mode only applies when O_CREAT actually creates the file
        // (POSIX ignores it for existing files).
        let created = if create {
            if excl {
                true
            } else {
                match self.nfs.resolve(path_bytes(&full)) {
                    Ok(_) => false,
                    Err(e) if e.status == nfsstat4_NFS4ERR_NOENT => true,
                    Err(e) => return Err(VfError::from_rpc(e, 0)),
                }
            }
        } else {
            false
        };
        // Open with NoCreate when the file already exists (kernel nfsd
        // rejects CREATE_GUARDED on existing files with NFS4ERR_EXIST).
        let (fh, stateid) = self.open_impl(&path_from_bytes(&dir), &name, access, created, excl)?;
        let setup = (|| {
            if created {
                self.nfs
                    .setattr(&fh, Some(mode & 0o7777), None)
                    .map_err(|e| VfError::from_rpc(e, 0))?;
            }
            if flags & O_TRUNC != 0 {
                self.nfs
                    .setattr(&fh, None, Some(0))
                    .map_err(|e| VfError::from_rpc(e, 0))?;
            }
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::BeforeRegister { index: 0 })?;
            Ok(())
        })();
        if let Err(error) = setup {
            let _ = self.nfs.close_path(&fh, &stateid);
            return Err(error);
        }
        let open = OpenFile {
            fh: fh.clone(),
            stateid,
            cur_offset: 0,
            append: flags & O_APPEND != 0,
            reopen: Some(ReopenFile {
                path: self.visible_path(&full),
                flags: non_destructive_reopen_flags(flags),
                mode,
            }),
        };
        match self.insert_open_file(open) {
            Ok(fd) => Ok(VfFile::from_fd(fd)),
            Err(error) => {
                let _ = self.nfs.close_path(&fh, &stateid);
                Err(error)
            }
        }
    }

    fn open_many(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<ManyResults<VfFile>> {
        if paths.len() != flags.len() || paths.len() != modes.len() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        self.drain_deferred_descriptor_closes()?;
        if paths.is_empty() {
            return Ok(ManyResults::all_success(Vec::new()));
        }
        self.openv_merged(paths, flags, modes)
    }

    fn before_open_cleanup(&mut self, _index: usize, _file: &VfFile) -> VfResult<()> {
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeCleanup { index: _index })?;
        Ok(())
    }

    fn close(&mut self, tcf: &VfFile) -> VfResult<()> {
        if !tcf.is_descriptor() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let fd = tcf.fd().unwrap();
        let open = self
            .open_files
            .remove(&fd)
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
        #[cfg(feature = "test-faults")]
        if let Err(error) = self.inject_open_fault(OpenFaultPoint::BeforeCloseDispatch { index: 0 })
        {
            self.deferred_descriptor_closes.push(open);
            return Err(error);
        }
        match self.nfs.close(&open.fh, &open.stateid) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.deferred_descriptor_closes.push(open);
                Err(VfError::from_rpc(error, 0))
            }
        }
    }

    fn sync_data(&mut self, tcf: &VfFile) -> VfResult<()> {
        let fd = tcf.fd().ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
        if self.open_files.contains_key(&fd) {
            // All writes request NFS FILE_SYNC4 stability. Validate the
            // descriptor so flush cannot incorrectly succeed after close.
            Ok(())
        } else {
            Err(VfError::failure(0, ERR_EBADF))
        }
    }

    fn closev(&mut self, files: &[VfFile]) -> VfRes {
        let mut ops = Vec::with_capacity(files.len());
        let mut fds = Vec::with_capacity(files.len());
        for (i, f) in files.iter().enumerate() {
            if !f.is_descriptor() {
                return Err(VfError::failure(i, nfsstat4_NFS4ERR_INVAL));
            }
            let fd = f.fd().unwrap();
            let open = self
                .open_files
                .get(&fd)
                .cloned()
                .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
            fds.push(fd);
            ops.push(crate::client::CloseOp {
                fh: open.fh,
                stateid: open.stateid,
            });
        }
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeCloseDispatch { index: 0 })?;
        #[cfg(feature = "test-faults")]
        for index in 0..ops.len() {
            if let Err(error) = self.inject_open_fault(OpenFaultPoint::BeforeCloseItem { index }) {
                if index > 0 {
                    self.nfs
                        .close_many(&ops[..index])
                        .map_err(VfError::from_rpc_indexed)?;
                    for fd in fds.iter().take(index) {
                        self.open_files.remove(fd);
                    }
                }
                return Err(error.with_index(index));
            }
        }
        match self.nfs.close_many(&ops) {
            Ok(()) => {
                for fd in fds {
                    self.open_files.remove(&fd);
                }
                Ok(())
            }
            Err(error) => {
                if !error.is_transport() {
                    for fd in fds.iter().take(error.op_index) {
                        self.open_files.remove(fd);
                    }
                }
                Err(VfError::from_rpc_indexed(error))
            }
        }
    }

    fn chdir(&mut self, path: &Path) -> VfResult<()> {
        let st = self.stat(path)?;
        if st.ftype != VfType::Directory {
            return Err(VfError::failure(0, ERR_NOTDIR));
        }
        self.cwd = self.abs_path(path);
        Ok(())
    }

    fn getcwd(&self) -> PathBuf {
        Path::new("/").join(&self.cwd)
    }

    fn readv(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| client.readv(reads));
        }
        if reads.is_empty() {
            return Ok(Vec::new());
        }
        if reads.iter().all(|r| r.file.is_descriptor()) {
            return self.readv_batch(reads);
        }
        // Path-based (and mixed descriptor/path) batches: resolve offsets and
        // try the merged compound.
        let mut path_ops = Vec::with_capacity(reads.len());
        let mut offsets = Vec::with_capacity(reads.len());
        for (i, r) in reads.iter().enumerate() {
            let off = match r.offset {
                VfOffset::At(o) => o,
                VfOffset::End => {
                    let path = self.server_vf_path(&r.file).map_err(|e| e.with_index(i))?;
                    let fh = self.resolve_follow(&path).map_err(|e| e.with_index(i))?;
                    self.file_size(&fh).map_err(|e| e.with_index(i))?
                }
                VfOffset::Cur => match r.file.fd() {
                    Some(fd) => self
                        .open_files
                        .get(&fd)
                        .map(|o| o.cur_offset)
                        .ok_or_else(|| VfError::failure(i, ERR_EBADF))?,
                    None => return Err(VfError::failure(i, ERR_INVAL)),
                },
                _ => return Err(VfError::failure(i, ERR_INVAL)),
            };
            let length =
                u64::try_from(r.length).map_err(|_| VfError::failure(i, libc::EOVERFLOW as u32))?;
            off.checked_add(length)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            offsets.push(off);
            let file = if r.file.is_descriptor() {
                let fd = r.file.fd().unwrap();
                let open = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
                crate::client::FileRef::Handle(open.fh.clone())
            } else {
                crate::client::FileRef::Path(
                    crate::path::path_bytes(
                        &self.server_vf_path(&r.file).map_err(|e| e.with_index(i))?,
                    )
                    .to_vec(),
                )
            };
            path_ops.push(crate::client::PathReadOp {
                file,
                offset: off,
                count: r.length,
                stateid: r
                    .file
                    .fd()
                    .and_then(|fd| self.open_files.get(&fd).map(|o| o.stateid)),
            });
        }
        let out = match self.merged_mode {
            MergedIoMode::Off => self.readv_path_fallback(reads),
            MergedIoMode::OpenWrite => self.readv_path_openwrite(reads, &path_ops, &offsets),
            MergedIoMode::Full => self.readv_path_full(reads, &path_ops, &offsets),
        }?;
        // Advance descriptor cursors for Cur-offset reads.
        for (i, r) in reads.iter().enumerate() {
            if r.offset == VfOffset::Cur && r.file.is_descriptor() {
                let new = out[i]
                    .offset
                    .checked_add(out[i].data.len() as u64)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                self.advance_offset(&r.file, new);
            }
        }
        Ok(out)
    }

    fn read_allv_with_options(
        &mut self,
        files: &[VfFile],
        options: ReadAllOptions,
    ) -> VfResult<Vec<Vec<u8>>> {
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| client.read_allv_with_options(files, options));
        }
        // Read until EOF in per-op chunks, batching every active file into
        // each compound so round trips scale with file size, not file count.
        // No size stat is needed: READ's EOF flag terminates each file.
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let per = self.nfs.read_per_op_bytes();
        // The window adapts to the number of active files: small files all
        // fit the first compound (so cat(20) is one compound), while big
        // files still get near-compound-sized windows.
        let compound_budget = if self.nfs.max_response_bytes > 0 {
            self.nfs.read_compound_bytes().saturating_sub(128)
        } else {
            per * 4
        };
        let max_window = per * (compound_budget / per).max(1);
        let mut out: Vec<Vec<u8>> = files.iter().map(|_| Vec::new()).collect();
        let mut offsets = vec![0u64; files.len()];
        let mut total = 0usize;
        let mut active: Vec<usize> = (0..files.len()).collect();
        while !active.is_empty() {
            let remaining = options.total_byte_limit().saturating_sub(total);
            let (batch_active, window) =
                bounded_read_allv_batch(&active, remaining, compound_budget, max_window);
            let reads: Vec<ReadOp> = batch_active
                .iter()
                .map(|&i| ReadOp::at(files[i].clone(), offsets[i], window))
                .collect();
            let results = self
                .readv(&reads)
                .map_err(|error| remap_active_error(error, &batch_active))?;
            let mut next = merge_read_allv_round(
                &batch_active,
                &results,
                &mut out,
                &mut offsets,
                &mut total,
                options.total_byte_limit(),
            )?;
            let mut untouched = active.split_off(batch_active.len());
            untouched.append(&mut next);
            active = untouched;
        }
        Ok(out)
    }

    fn writev(&mut self, writes: &[WriteOp]) -> VfResult<Vec<WriteResult>> {
        if writes.is_empty() {
            return Ok(Vec::new());
        }
        if writes.iter().all(|w| w.file.is_descriptor()) {
            return self.writev_batch(writes);
        }
        let mut path_ops = Vec::with_capacity(writes.len());
        let mut offsets = Vec::with_capacity(writes.len());
        for (i, w) in writes.iter().enumerate() {
            let off = match w.offset {
                VfOffset::At(o) => o,
                VfOffset::End => {
                    let path = self.server_vf_path(&w.file).map_err(|e| e.with_index(i))?;
                    let fh = self.resolve_follow(&path).map_err(|e| e.with_index(i))?;
                    self.file_size(&fh).map_err(|e| e.with_index(i))?
                }
                VfOffset::Cur => match w.file.fd() {
                    Some(fd) => self
                        .open_files
                        .get(&fd)
                        .map(|o| o.cur_offset)
                        .ok_or_else(|| VfError::failure(i, ERR_EBADF))?,
                    None => return Err(VfError::failure(i, ERR_INVAL)),
                },
                _ => return Err(VfError::failure(i, ERR_INVAL)),
            };
            let length = u64::try_from(w.data.len())
                .map_err(|_| VfError::failure(i, libc::EOVERFLOW as u32))?;
            off.checked_add(length)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            offsets.push(off);
            let file = if w.file.is_descriptor() {
                let fd = w.file.fd().unwrap();
                let open = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
                crate::client::FileRef::Handle(open.fh.clone())
            } else {
                crate::client::FileRef::Path(
                    crate::path::path_bytes(
                        &self.server_vf_path(&w.file).map_err(|e| e.with_index(i))?,
                    )
                    .to_vec(),
                )
            };
            path_ops.push(crate::client::PathWriteOp {
                file,
                offset: off,
                data: w.data.clone(),
                create: w.creation && !w.file.is_descriptor(),
                truncate: w.truncate && !w.file.is_descriptor(),
                stateid: w
                    .file
                    .fd()
                    .and_then(|fd| self.open_files.get(&fd).map(|o| o.stateid)),
            });
        }
        let out = match self.merged_mode {
            MergedIoMode::Off => self.writev_path_fallback(writes),
            MergedIoMode::OpenWrite => self.writev_path_openwrite(writes, &path_ops, &offsets),
            MergedIoMode::Full => self.writev_path_full(writes, &path_ops, &offsets),
        }?;
        // Advance descriptor cursors for Cur-offset writes.
        for (i, w) in writes.iter().enumerate() {
            if w.offset == VfOffset::Cur && w.file.is_descriptor() {
                let new = out[i]
                    .offset
                    .checked_add(out[i].written as u64)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                self.advance_offset(&w.file, new);
            }
        }
        Ok(out)
    }

    fn fseek(&mut self, tcf: &VfFile, offset: i64, whence: SeekFrom) -> VfResult<i64> {
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| client.fseek(tcf, offset, whence));
        }
        if !tcf.is_descriptor() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let cur = self
            .open_files
            .get(&tcf.fd().unwrap())
            .map(|o| o.cur_offset)
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
        let new = match whence {
            SeekFrom::Set => offset,
            SeekFrom::Cur => i64::try_from(cur)
                .ok()
                .and_then(|cur| cur.checked_add(offset))
                .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?,
            SeekFrom::End => {
                let fh = self
                    .open_files
                    .get(&tcf.fd().unwrap())
                    .map(|o| o.fh.clone())
                    .unwrap();
                let size = self.file_size(&fh)?;
                i64::try_from(size)
                    .ok()
                    .and_then(|size| size.checked_add(offset))
                    .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?
            }
            _ => return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL)),
        };
        if new < 0 {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let new = new as u64;
        self.advance_offset(tcf, new);
        Ok(new as i64)
    }

    fn getattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| client.getattrsv(attrs));
        }
        self.getattrsv_impl(attrs, true)
    }

    fn lgetattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| client.lgetattrsv(attrs));
        }
        self.getattrsv_impl(attrs, false)
    }

    fn setattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes {
        self.setattrsv_impl(attrs, true)
    }

    fn lsetattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes {
        self.setattrsv_impl(attrs, false)
    }

    fn listdir(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> VfResult<Vec<VfAttrs>> {
        if !self.recovery_in_progress {
            return self
                .read_with_recovery(|client| client.listdir(dir, masks, max_count, recursive));
        }
        let mut out = Vec::new();
        self.listdir_rec(dir, masks, max_count, recursive, &mut out)?;
        Ok(out)
    }

    /// List many directories in a few compounds: the directories are
    /// resolved in one batched lookup, then their first READDIR pages (and
    /// any continuation pages) are drained in batched compounds — mirroring
    /// the txn-compound client's 64-READDIRs-per-compound listdirv.
    fn listdirv(
        &mut self,
        dirs: &[&Path],
        masks: AttrMask,
        max_entries: usize,
        recursive: bool,
        cb: &mut dyn FnMut(&VfAttrs, &Path) -> bool,
    ) -> VfRes {
        if dirs.is_empty() {
            return Ok(());
        }
        let ids = request_mask_to_attr_list(&masks);
        let mut counted = 0usize;
        let mut level_paths: Vec<PathBuf> = dirs.iter().map(|d| d.to_path_buf()).collect();
        let mut level_owners: Vec<usize> = (0..dirs.len()).collect();
        loop {
            if level_paths.is_empty() {
                return Ok(());
            }
            // Batch-resolve this level's directories.
            let files: Vec<VfFile> = level_paths
                .iter()
                .map(|p| VfFile::from_os_path(p))
                .collect();
            let refs: Vec<&VfFile> = files.iter().collect();
            let resolved = self.resolve_many_tcfile(&refs, true)?;
            let mut level: Vec<(FileHandle, PathBuf, usize)> =
                Vec::with_capacity(level_paths.len());
            for (i, r) in resolved.iter().enumerate() {
                let owner = level_owners[i];
                match r {
                    Ok((fh, ftype)) if *ftype == nfs_ftype4_NF4DIR => {
                        level.push((fh.clone(), level_paths[i].clone(), owner));
                    }
                    Ok((_, _)) => {
                        return Err(VfError::failure(owner, nfsstat4_NFS4ERR_NOTDIR));
                    }
                    Err(status) => return Err(VfError::nfs(owner, *status)),
                }
            }
            // First pages for all directories in one compound.
            let ops: Vec<(FileHandle, u64)> =
                level.iter().map(|(fh, _, _)| (fh.clone(), 0)).collect();
            let results = self.nfs.readdir_pages(&ops, &ids).map_err(|error| {
                remap_active_error(VfError::from_rpc_indexed(error), &level_owners)
            })?;
            let mut accumulated: Vec<Vec<crate::client::DirEntry>> =
                results.iter().map(|r| r.0.clone()).collect();
            let mut pending: Vec<(usize, FileHandle, u64)> = results
                .iter()
                .enumerate()
                .filter(|(_, r)| r.1 != 0)
                .map(|(i, r)| (i, level[i].0.clone(), r.1))
                .collect();
            // Drain continuation pages, batched across directories.
            while !pending.is_empty() {
                let cont_ops: Vec<(FileHandle, u64)> = pending
                    .iter()
                    .map(|(_, fh, cookie)| (fh.clone(), *cookie))
                    .collect();
                let pending_owners: Vec<usize> =
                    pending.iter().map(|(idx, _, _)| level[*idx].2).collect();
                let cont = self.nfs.readdir_pages(&cont_ops, &ids).map_err(|error| {
                    remap_active_error(VfError::from_rpc_indexed(error), &pending_owners)
                })?;
                let mut next_pending = Vec::new();
                for ((idx, fh, _), (entries, cookie)) in pending.iter().zip(cont) {
                    accumulated[*idx].extend(entries);
                    if cookie != 0 {
                        next_pending.push((*idx, fh.clone(), cookie));
                    }
                }
                pending = next_pending;
            }
            // Emit entries and collect subdirectories for the next level.
            let mut next_level: Vec<PathBuf> = Vec::new();
            let mut next_owners: Vec<usize> = Vec::new();
            for (idx, entries) in accumulated.iter().enumerate() {
                let dir = &level[idx].1;
                let owner = level[idx].2;
                for e in entries {
                    if max_entries != 0 && counted >= max_entries {
                        return Ok(());
                    }
                    let path = dir.join(path_from_bytes(&e.name));
                    let mut a = VfAttrs {
                        file: VfFile::from_os_path(&path),
                        masks,
                        ..VfAttrs::default()
                    };
                    let vals =
                        parse_attr_list(&ids, &e.attrs).map_err(|error| error.with_index(owner))?;
                    apply_attrs(&mut a, &vals);
                    if recursive && a.ftype == VfType::Directory {
                        next_level.push(path);
                        next_owners.push(owner);
                    }
                    if !cb(&a, dir) {
                        return Ok(());
                    }
                    counted += 1;
                }
            }
            if !recursive {
                return Ok(());
            }
            level_paths = next_level;
            level_owners = next_owners;
        }
    }

    fn walk_with_options(
        &mut self,
        root: &Path,
        masks: AttrMask,
        options: WalkOptions,
        sort: &mut dyn FnMut(&Path, &mut Vec<VfAttrs>),
    ) -> VfResult<Vec<WalkEntry>> {
        let root_fh = self.resolve_path(&self.server_path(root), true)?;
        let ids = request_mask_to_attr_list(&masks);
        let mut collected: std::collections::HashMap<PathBuf, Vec<VfAttrs>> =
            std::collections::HashMap::new();
        let mut entry_count = 0usize;
        let mut stored_path_bytes = 0usize;

        // Resolve the root once, then decode each bounded READDIR response
        // directly into the caller-visible accumulator. Raw continuation
        // pages are never retained after they are decoded.
        let mut root_attrs = Vec::new();
        let mut cookie = 0u64;
        loop {
            let page = self
                .nfs
                .readdir(&root_fh, cookie, &ids)
                .map_err(|error| VfError::from_rpc(error, 0))?;
            append_bounded_walk_page(
                root,
                root,
                masks,
                &ids,
                &page,
                options,
                &mut entry_count,
                &mut stored_path_bytes,
                &mut root_attrs,
            )?;
            cookie = page.last().map(|entry| entry.cookie).unwrap_or(0);
            if cookie == 0 {
                break;
            }
        }
        sort(root, &mut root_attrs);

        // Preserve the parent filehandle so resolving and reading each child
        // directory can share one compound instead of re-walking full paths.
        let mut frontier: Vec<(FileHandle, PathBuf)> = root_attrs
            .iter()
            .filter(|entry| entry.ftype == VfType::Directory)
            .filter_map(|entry| {
                entry
                    .file
                    .path()
                    .map(|path| (root_fh.clone(), path.to_path_buf()))
            })
            .collect();
        collected.insert(root.to_path_buf(), root_attrs);

        while !frontier.is_empty() {
            let operations: Vec<(FileHandle, Vec<u8>)> = frontier
                .iter()
                .map(|(parent, path)| {
                    (
                        parent.clone(),
                        path.file_name()
                            .map(|name| path_bytes(Path::new(name)).to_vec())
                            .unwrap_or_default(),
                    )
                })
                .collect();
            let results = self
                .nfs
                .readdir_children(&operations, &ids)
                .map_err(VfError::from_rpc_indexed)?;
            let mut level_attrs: Vec<Vec<VfAttrs>> =
                (0..results.len()).map(|_| Vec::new()).collect();
            let mut pending = Vec::new();
            for (index, result) in results.iter().enumerate() {
                append_bounded_walk_page(
                    root,
                    &frontier[index].1,
                    masks,
                    &ids,
                    &result.entries,
                    options,
                    &mut entry_count,
                    &mut stored_path_bytes,
                    &mut level_attrs[index],
                )?;
                if result.cookie != 0 {
                    pending.push((index, result.fh.clone(), result.cookie));
                }
            }

            while !pending.is_empty() {
                let operations: Vec<(FileHandle, u64)> = pending
                    .iter()
                    .map(|(_, handle, cookie)| (handle.clone(), *cookie))
                    .collect();
                let pages = self
                    .nfs
                    .readdir_pages(&operations, &ids)
                    .map_err(VfError::from_rpc_indexed)?;
                let mut next_pending = Vec::new();
                for ((index, handle, _), (page, cookie)) in pending.iter().zip(pages) {
                    append_bounded_walk_page(
                        root,
                        &frontier[*index].1,
                        masks,
                        &ids,
                        &page,
                        options,
                        &mut entry_count,
                        &mut stored_path_bytes,
                        &mut level_attrs[*index],
                    )?;
                    if cookie != 0 {
                        next_pending.push((*index, handle.clone(), cookie));
                    }
                }
                pending = next_pending;
            }

            let mut next_frontier = Vec::new();
            for (index, mut entries) in level_attrs.into_iter().enumerate() {
                let directory = frontier[index].1.clone();
                sort(&directory, &mut entries);
                for entry in &entries {
                    if entry.ftype == VfType::Directory
                        && let Some(path) = entry.file.path()
                        && path
                            .strip_prefix(root)
                            .map(|relative| relative.components().count())
                            .unwrap_or(usize::MAX)
                            <= options.depth_limit()
                    {
                        next_frontier.push((results[index].fh.clone(), path.to_path_buf()));
                    }
                }
                collected.insert(directory, entries);
            }
            frontier = next_frontier;
        }

        let mut out = Vec::with_capacity(collected.len());
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = collected.remove(&dir).unwrap_or_default();
            let subs: Vec<PathBuf> = entries
                .iter()
                .filter(|e| e.ftype == VfType::Directory)
                .filter_map(|e| e.file.path().map(Path::to_path_buf))
                .collect();
            for s in subs.into_iter().rev() {
                stack.push(s);
            }
            out.push(WalkEntry { path: dir, entries });
        }
        Ok(out)
    }

    fn renamev(&mut self, pairs: &[(VfFile, VfFile)]) -> VfRes {
        if pairs.is_empty() {
            return Ok(());
        }
        if pairs
            .iter()
            .any(|(s, d)| s.is_descriptor() || d.is_descriptor())
        {
            return self.renamev_phased(pairs);
        }
        let mut prs = Vec::with_capacity(pairs.len());
        for (i, (src, dst)) in pairs.iter().enumerate() {
            let s = self.server_vf_path(src).map_err(|e| e.with_index(i))?;
            let d = self.server_vf_path(dst).map_err(|e| e.with_index(i))?;
            prs.push(crate::client::PathRenamePair {
                src: path_bytes(&s).to_vec(),
                dst: path_bytes(&d).to_vec(),
            });
        }
        let outcome = self
            .nfs
            .renamev_path_compound(&prs)
            .map_err(|e| VfError::from_rpc(e, None))?;
        match outcome.failed {
            Some((i, _st)) => {
                // Prefix [0..i) renamed; retry [i..] via the phased path,
                // re-attributing its error to the original index.
                let suffix = &pairs[i..];
                self.renamev_phased(suffix)
                    .map_err(|e| e.map_index(|rel| i + rel))
            }
            None => Ok(()),
        }
    }

    fn removev(&mut self, files: &[VfFile]) -> VfRes {
        if files.is_empty() {
            return Ok(());
        }
        if files.iter().any(|f| f.is_descriptor()) {
            return self.removev_phased(files);
        }
        let mut paths = Vec::with_capacity(files.len());
        for (i, f) in files.iter().enumerate() {
            paths.push(path_bytes(&self.server_vf_path(f).map_err(|e| e.with_index(i))?).to_vec());
        }
        let outcome = self
            .nfs
            .removev_path_compound(&paths)
            .map_err(|e| VfError::from_rpc(e, None))?;
        match outcome.failed {
            Some((i, _st)) => {
                // Prefix [0..i) removed; retry [i..] via the phased path.
                let suffix = &files[i..];
                self.removev_phased(suffix)
                    .map_err(|e| e.map_index(|rel| i + rel))
            }
            None => Ok(()),
        }
    }

    fn mkdirv(&mut self, dirs: &[VfAttrs]) -> VfRes {
        if dirs.is_empty() {
            return Ok(());
        }
        // Batch-resolve the parents, then CREATE in one compound.
        let mut parents: Vec<PathBuf> = Vec::with_capacity(dirs.len());
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(dirs.len());
        for (i, a) in dirs.iter().enumerate() {
            let path = self.server_vf_path(&a.file).map_err(|e| e.with_index(i))?;
            let (dir, name) =
                split_path_bytes(path_bytes(&path)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            parents.push(self.visible_path(&path_from_bytes(&dir)));
            names.push(name);
        }
        let files: Vec<VfFile> = parents.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let resolved = self.resolve_many_tcfile(&refs, true)?;
        let mut creates = Vec::with_capacity(dirs.len());
        for (i, (r, name)) in resolved.iter().zip(&names).enumerate() {
            match r {
                Ok((fh, ftype)) if *ftype == nfs_ftype4_NF4DIR => {
                    creates.push(crate::client::CreateOp {
                        dir: fh.clone(),
                        name: name.clone(),
                        ftype: nfs_ftype4_NF4DIR,
                        linkdata: None,
                    });
                }
                Ok((_, _)) => return Err(VfError::failure(i, nfsstat4_NFS4ERR_NOTDIR)),
                Err(status) => return Err(VfError::nfs(i, *status)),
            }
        }
        if let Err(e) = self.nfs.create_many(&creates) {
            let i = e.op_index;
            // The prefix [0..i) was created; apply its modes before
            // reporting the failure (modes cannot ride in the CREATE).
            if i > 0 {
                let _ = self.apply_dir_modes(&dirs[..i]);
            }
            return Err(VfError::from_rpc_indexed(e));
        }
        self.apply_dir_modes(dirs)
    }

    fn symlinkv(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        // Batch-resolve the destination parents, then CREATE in one compound.
        let mut parents: Vec<PathBuf> = Vec::with_capacity(newpaths.len());
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(newpaths.len());
        for (i, new) in newpaths.iter().enumerate() {
            let full = self.server_path(new);
            let (dir, name) =
                split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            parents.push(self.visible_path(&path_from_bytes(&dir)));
            names.push(name);
        }
        let files: Vec<VfFile> = parents.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let resolved = self.resolve_many_tcfile(&refs, true)?;
        let mut ops = Vec::with_capacity(oldpaths.len());
        for (i, ((r, name), old)) in resolved.iter().zip(&names).zip(oldpaths).enumerate() {
            match r {
                Ok((fh, ftype)) if *ftype == nfs_ftype4_NF4DIR => {
                    ops.push(crate::client::CreateOp {
                        dir: fh.clone(),
                        name: name.clone(),
                        ftype: nfs_ftype4_NF4LNK,
                        linkdata: Some(path_bytes(old).to_vec()),
                    })
                }
                Ok((_, _)) => return Err(VfError::failure(i, nfsstat4_NFS4ERR_NOTDIR)),
                Err(status) => return Err(VfError::nfs(i, *status)),
            }
        }
        self.nfs
            .create_many(&ops)
            .map_err(VfError::from_rpc_indexed)
    }

    fn readlinkv(&mut self, paths: &[&Path]) -> VfResult<Vec<Vec<u8>>> {
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| client.readlinkv(paths));
        }
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        // Batch-resolve the links themselves (no final-component follow),
        // then READLINK in one compound.
        let files: Vec<VfFile> = paths.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let resolved = self.resolve_many_tcfile(&refs, false)?;
        let mut ops = Vec::with_capacity(paths.len());
        for (i, r) in resolved.iter().enumerate() {
            match r {
                Ok((fh, _)) => ops.push(crate::client::ReadlinkOp { fh: fh.clone() }),
                Err(status) => return Err(VfError::nfs(i, *status)),
            }
        }
        self.nfs
            .readlink_many(&ops)
            .map_err(VfError::from_rpc_indexed)
    }

    fn hardlinkv(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        if oldpaths.is_empty() {
            return Ok(());
        }
        // Batch-resolve the sources (no follow) and destination parents
        // (follow), then LINK in one compound.
        let src_files: Vec<VfFile> = oldpaths.iter().map(|p| VfFile::from_os_path(p)).collect();
        let src_refs: Vec<&VfFile> = src_files.iter().collect();
        let src_resolved = self.resolve_many_tcfile(&src_refs, false)?;
        let mut parents: Vec<PathBuf> = Vec::with_capacity(newpaths.len());
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(newpaths.len());
        for (i, new) in newpaths.iter().enumerate() {
            let full = self.server_path(new);
            let (dir, name) =
                split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            parents.push(self.visible_path(&path_from_bytes(&dir)));
            names.push(name);
        }
        let files: Vec<VfFile> = parents.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let dst_resolved = self.resolve_many_tcfile(&refs, true)?;
        let mut ops = Vec::with_capacity(oldpaths.len());
        for (i, (((sr, dr), name), _old)) in src_resolved
            .iter()
            .zip(&dst_resolved)
            .zip(&names)
            .zip(oldpaths)
            .enumerate()
        {
            match (sr, dr) {
                (Ok((src, _)), Ok((dstdir, ftype))) if *ftype == nfs_ftype4_NF4DIR => {
                    ops.push(crate::client::LinkOp {
                        dstdir: dstdir.clone(),
                        src: src.clone(),
                        newname: name.clone(),
                    });
                }
                (Ok((_, _)), Ok((_, _))) => {
                    return Err(VfError::failure(i, nfsstat4_NFS4ERR_NOTDIR));
                }
                (Err(status), _) => return Err(VfError::nfs(i, *status)),
                (_, Err(status)) => return Err(VfError::nfs(i, *status)),
            }
        }
        self.nfs.link_many(&ops).map_err(VfError::from_rpc_indexed)
    }

    fn dupv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        for (i, p) in pairs.iter().enumerate() {
            // Follow final-component symlinks for both ends, matching the
            // `std::fs` backend (OPEN cannot target a symlink directly).
            let src = self
                .follow_target_path(&self.server_path(&p.src_path))
                .map_err(|e| e.with_index(i))?;
            let dst = self
                .follow_target_path(&self.server_path(&p.dst_path))
                .map_err(|e| e.with_index(i))?;
            self.copy_extent(&src, &dst, p)
                .map_err(|e| e.with_index(i))?;
        }
        Ok(())
    }

    fn lcopyv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        for (i, p) in pairs.iter().enumerate() {
            let src = self.lstat(&p.src_path).map_err(|e| e.with_index(i))?;
            if src.ftype == VfType::Symlink {
                let target = self.readlink(&p.src_path).map_err(|e| e.with_index(i))?;
                let target_path = path_from_bytes(&target);
                self.symlink(&target_path, &p.dst_path)
                    .map_err(|e| e.with_index(i))?;
            } else {
                let dst = self
                    .follow_target_path(&self.server_path(&p.dst_path))
                    .map_err(|e| e.with_index(i))?;
                self.copy_extent(&self.server_path(&p.src_path), &dst, p)
                    .map_err(|e| e.with_index(i))?;
            }
        }
        Ok(())
    }

    fn copyv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        if !self.server_copy_enabled {
            return self.dupv(pairs);
        }
        // Keep the number of simultaneously open source/destination states
        // bounded while still amortizing OPEN, COPY, SETATTR, and CLOSE.
        const FILES_PER_COPY_BATCH: usize = 8;
        for (base, batch) in pairs.chunks(FILES_PER_COPY_BATCH).enumerate() {
            if let Err(e) = self.copy_extents_server_side(batch) {
                if matches!(
                    e.err_no(),
                    nfsstat4_NFS4ERR_NOTSUPP
                        | nfsstat4_NFS4ERR_OP_ILLEGAL
                        | nfsstat4_NFS4ERR_OFFLOAD_DENIED
                        | nfsstat4_NFS4ERR_OFFLOAD_NO_REQS
                        | nfsstat4_NFS4ERR_STALE_STATEID
                        | nfsstat4_NFS4ERR_OLD_STATEID
                        | nfsstat4_NFS4ERR_BAD_STATEID
                ) {
                    self.server_copy_enabled = false;
                    self.server_copy_stats.fallbacks += 1;
                    let start = base * FILES_PER_COPY_BATCH;
                    return self
                        .dupv(&pairs[start..])
                        .map_err(|fallback| fallback.map_index(|index| start + index));
                }
                return Err(e.map_index(|index| base * FILES_PER_COPY_BATCH + index));
            }
        }
        Ok(())
    }

    fn write_adb(&mut self, patterns: &[Adb]) -> VfResult<Vec<usize>> {
        let mut counts = Vec::with_capacity(patterns.len());
        for (i, p) in patterns.iter().enumerate() {
            // Validate the entire layout before creating/opening a file so an
            // overflow cannot strand server-side open state.
            let mut layout = Vec::with_capacity(p.adb_block_count);
            let pattern_len = u64::try_from(p.adb_pattern_data.len())
                .map_err(|_| VfError::failure(i, libc::EOVERFLOW as u32))?;
            for b in 0..p.adb_block_count {
                let base = adb_block_base(p, b, i)?;
                let block_number = p
                    .adb_block_num
                    .checked_add(b as u64)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                let number_offset = p
                    .adb_reloff_blocknum
                    .map(|relative| {
                        let offset = adb_field_offset(base, relative, i)?;
                        adb_field_offset(offset, 8, i)?;
                        Ok(offset)
                    })
                    .transpose()?;
                let pattern_offset = p
                    .adb_reloff_pattern
                    .map(|relative| {
                        let offset = adb_field_offset(base, relative, i)?;
                        adb_field_offset(offset, pattern_len, i)?;
                        Ok(offset)
                    })
                    .transpose()?;
                layout.push((block_number, number_offset, pattern_offset));
            }
            let full = self.server_path(&p.path);
            let (dir, name) = match split_path_bytes(path_bytes(&full)) {
                Ok(x) => x,
                Err(_) => return Err(VfError::failure(i, ERR_NOENT)),
            };
            let dirfh = match self.resolve_path(&path_from_bytes(&dir), true) {
                Ok(fh) => fh,
                Err(e) => return Err(e.with_index(i)),
            };
            let (fh, sid) = match self
                .nfs
                .open_path(
                    &dirfh,
                    &name,
                    OPEN4_SHARE_ACCESS_WRITE,
                    crate::client::OpenCreate::Guarded,
                )
                .map_err(|e| VfError::from_rpc(e, 0))
            {
                Ok(x) => x,
                Err(e) => return Err(e.with_index(i)),
            };
            let mut written = 0usize;
            let mut failed: Option<VfError> = None;
            for (block_number, number_offset, pattern_offset) in layout {
                if let Some(offset) = number_offset {
                    let adbn = block_number.to_be_bytes();
                    if let Err(e) = self.nfs.write(&fh, &sid, offset, &adbn) {
                        failed = Some(VfError::from_rpc(e, i));
                        break;
                    }
                }
                if let Some(offset) = pattern_offset
                    && !p.adb_pattern_data.is_empty()
                    && let Err(e) = self.nfs.write(&fh, &sid, offset, &p.adb_pattern_data)
                {
                    failed = Some(VfError::from_rpc(e, i));
                    break;
                }
                written += 1;
            }
            let _ = self.nfs.close_path(&fh, &sid);
            if let Some(e) = failed {
                return Err(e);
            }
            counts.push(written);
        }
        Ok(counts)
    }

    fn rm(&mut self, objs: &[&Path], recursive: bool) -> VfRes {
        for (i, o) in objs.iter().enumerate() {
            self.rm_one(o, recursive).map_err(|e| e.with_index(i))?;
        }
        Ok(())
    }

    fn cp_recursive(
        &mut self,
        src_dir: &Path,
        dst: &Path,
        symlinks: bool,
        _use_server_side_copy: bool,
    ) -> VfRes {
        if !self.exists(dst)? {
            self.ensure_dir(dst, 0o755).map_err(|e| e.with_index(0))?;
        }
        let masks = AttrMask::MODE | AttrMask::SIZE | AttrMask::FILEID;
        let mut pending = vec![(src_dir.to_path_buf(), dst.to_path_buf())];
        while let Some((source, destination)) = pending.pop() {
            let entries = self.listdir(&source, masks, 0, false)?;
            let mut directories = Vec::new();
            for entry in entries {
                let name = entry
                    .file
                    .path()
                    .and_then(|path| path.file_name())
                    .map(|name| path_bytes(Path::new(name)).to_vec())
                    .ok_or_else(|| VfError::failure(0, nfsstat4_NFS4ERR_INVAL))?;
                let source_child = source.join(path_from_bytes(&name));
                let destination_child = destination.join(path_from_bytes(&name));
                if entry.ftype == VfType::Directory {
                    self.ensure_dir(&destination_child, 0o755)
                        .map_err(|error| error.with_index(0))?;
                    directories.push((source_child, destination_child));
                } else if entry.ftype == VfType::Symlink && symlinks {
                    let target = self
                        .readlink(&source_child)
                        .map_err(|error| error.with_index(0))?;
                    self.symlink(&path_from_bytes(&target), &destination_child)
                        .map_err(|error| error.with_index(0))?;
                } else {
                    let pair =
                        ExtentPair::from_os_paths(&source_child, 0, &destination_child, 0, None);
                    let source_target = self
                        .follow_target_path(&self.server_path(&source_child))
                        .map_err(|error| error.with_index(0))?;
                    let destination_target = self
                        .follow_target_path(&self.server_path(&destination_child))
                        .map_err(|error| error.with_index(0))?;
                    self.copy_extent(&source_target, &destination_target, &pair)
                        .map_err(|error| error.with_index(0))?;
                }
            }
            for directory in directories.into_iter().rev() {
                pending.push(directory);
            }
        }
        Ok(())
    }
}

impl Drop for NfsVecFs {
    /// `tc_deinit()`: close every open file so the client has no state left,
    /// allowing the session teardown to destroy the clientid on the server.
    fn drop(&mut self) {
        let closes: Vec<crate::client::CloseOp> = self
            .open_files
            .drain()
            .map(|(_, open)| open)
            .chain(self.deferred_descriptor_closes.drain(..))
            .map(|open| crate::client::CloseOp {
                fh: open.fh,
                stateid: open.stateid,
            })
            .collect();
        let _ = self.nfs.close_many(&closes);
    }
}

// ---------------------------------------------------------------------------
// NFS attribute parsing
// ---------------------------------------------------------------------------

/// Parsed values of a GETATTR reply for the supported FATTR4 attributes.
#[derive(Debug, Clone, Default)]
struct AttrValues {
    ftype: Option<u32>,
    change: Option<u64>,
    mode: Option<u32>,
    size: Option<u64>,
    nlink: Option<u32>,
    fileid: Option<u64>,
    uid: Option<u32>,
    gid: Option<u32>,
    rdev: Option<u64>,
    blocks: Option<u64>,
    mtime: Option<(i64, u32)>,
    atime: Option<(i64, u32)>,
    ctime: Option<(i64, u32)>,
    has_named_attr: Option<bool>,
}

/// The full set of supported FATTR4 ids, in wire (increasing) order. Must
/// match `crate::client::READDIR_ATTRS`.
const FULL_ATTR_IDS: [u32; 14] = [
    FATTR4_TYPE,
    FATTR4_CHANGE,
    FATTR4_SIZE,
    FATTR4_NAMED_ATTR,
    FATTR4_FILEID,
    FATTR4_MODE,
    FATTR4_NUMLINKS,
    FATTR4_OWNER,
    FATTR4_OWNER_GROUP,
    FATTR4_RAWDEV,
    FATTR4_SPACE_USED,
    FATTR4_TIME_ACCESS,
    FATTR4_TIME_METADATA,
    FATTR4_TIME_MODIFY,
];

fn request_mask_to_attr_list(masks: &AttrMask) -> Vec<u32> {
    let mut ids = Vec::new();
    for id in FULL_ATTR_IDS {
        let wanted = match id {
            FATTR4_TYPE => true, // always fetch type (cheap, aids listdir)
            FATTR4_CHANGE => masks.contains(AttrMask::CHANGE),
            FATTR4_SIZE => masks.contains(AttrMask::SIZE),
            FATTR4_NAMED_ATTR => masks.contains(AttrMask::NAMED_ATTR),
            FATTR4_FILEID => masks.contains(AttrMask::FILEID),
            FATTR4_MODE => masks.contains(AttrMask::MODE),
            FATTR4_NUMLINKS => masks.contains(AttrMask::NLINK),
            FATTR4_OWNER => masks.contains(AttrMask::UID),
            FATTR4_OWNER_GROUP => masks.contains(AttrMask::GID),
            FATTR4_RAWDEV => masks.contains(AttrMask::RDEV),
            FATTR4_SPACE_USED => masks.contains(AttrMask::BLOCKS),
            FATTR4_TIME_ACCESS => masks.contains(AttrMask::ATIME),
            FATTR4_TIME_METADATA => masks.contains(AttrMask::CTIME),
            FATTR4_TIME_MODIFY => masks.contains(AttrMask::MTIME),
            _ => false,
        };
        if wanted {
            ids.push(id);
        }
    }
    ids
}

/// Parse a raw GETATTR attribute list encoded for the given ids (in id order).
fn parse_attr_list(ids: &[u32], list: &[u8]) -> VfResult<AttrValues> {
    let mut v = AttrValues::default();
    let mut off = 0usize;
    for id in ids {
        match *id {
            FATTR4_TYPE => {
                v.ftype = Some(read_u32(list, &mut off)?);
            }
            FATTR4_CHANGE => {
                v.change = Some(read_u64(list, &mut off)?);
            }
            FATTR4_SIZE => {
                v.size = Some(read_u64(list, &mut off)?);
            }
            FATTR4_NAMED_ATTR => {
                v.has_named_attr = Some(read_u32(list, &mut off)? != 0);
            }
            FATTR4_FILEID => {
                v.fileid = Some(read_u64(list, &mut off)?);
            }
            FATTR4_MODE => {
                v.mode = Some(read_u32(list, &mut off)?);
            }
            FATTR4_NUMLINKS => {
                v.nlink = Some(read_u32(list, &mut off)?);
            }
            FATTR4_OWNER => {
                v.uid = crate::identity::name_to_id(&read_str(list, &mut off)?, false);
            }
            FATTR4_OWNER_GROUP => {
                v.gid = crate::identity::name_to_id(&read_str(list, &mut off)?, true);
            }
            FATTR4_RAWDEV => {
                let major = read_u32(list, &mut off)?;
                let minor = read_u32(list, &mut off)?;
                v.rdev = Some(libc::makedev(major, minor));
            }
            FATTR4_SPACE_USED => {
                v.blocks = Some(read_u64(list, &mut off)? / 512);
            }
            FATTR4_TIME_ACCESS => {
                v.atime = Some(read_nfstime(list, &mut off)?);
            }
            FATTR4_TIME_MODIFY => {
                v.mtime = Some(read_nfstime(list, &mut off)?);
            }
            FATTR4_TIME_METADATA => {
                v.ctime = Some(read_nfstime(list, &mut off)?);
            }
            _ => unreachable!(),
        }
    }
    if off != list.len() {
        return Err(attr_decode_error());
    }
    Ok(v)
}

#[cfg(feature = "fuzzing")]
pub(crate) fn validate_attr_list(ids: &[u32], list: &[u8]) -> VfResult<()> {
    parse_attr_list(ids, list).map(|_| ())
}

/// `S_IFMT` type bits for an NFSv4 file type code.
fn s_ifmt(ftype: u32) -> u32 {
    match ftype {
        nfs_ftype4_NF4REG => 0o100000,  // S_IFREG
        nfs_ftype4_NF4DIR => 0o040000,  // S_IFDIR
        nfs_ftype4_NF4LNK => 0o120000,  // S_IFLNK
        nfs_ftype4_NF4BLK => 0o060000,  // S_IFBLK
        nfs_ftype4_NF4CHR => 0o020000,  // S_IFCHR
        nfs_ftype4_NF4FIFO => 0o010000, // S_IFIFO
        nfs_ftype4_NF4SOCK => 0o140000, // S_IFSOCK
        _ => 0,
    }
}

/// Fill `a` from parsed values where the mask requests the attribute. `mode`
/// is the permission bits plus the `S_IFMT` bits derived from `ftype`.
fn apply_attrs(a: &mut VfAttrs, v: &AttrValues) {
    a.ftype = v.ftype.map(VfType::from_nfs).unwrap_or(VfType::Regular);
    a.returned = AttrMask::empty();
    if a.masks.contains(AttrMask::MODE)
        && let Some(mode) = v.mode
    {
        a.mode = mode | s_ifmt(a.ftype.as_nfs());
        a.returned.insert(AttrMask::MODE);
    }
    if a.masks.contains(AttrMask::SIZE)
        && let Some(size) = v.size
    {
        a.size = size;
        a.returned.insert(AttrMask::SIZE);
    }
    if a.masks.contains(AttrMask::NLINK)
        && let Some(nlink) = v.nlink
    {
        a.nlink = nlink;
        a.returned.insert(AttrMask::NLINK);
    }
    if a.masks.contains(AttrMask::FILEID)
        && let Some(fileid) = v.fileid
    {
        a.fileid = fileid;
        a.returned.insert(AttrMask::FILEID);
    }
    if a.masks.contains(AttrMask::CHANGE)
        && let Some(change) = v.change
    {
        a.change = change;
        a.returned.insert(AttrMask::CHANGE);
    }
    if a.masks.contains(AttrMask::UID)
        && let Some(uid) = v.uid
    {
        a.uid = uid;
        a.returned.insert(AttrMask::UID);
    }
    if a.masks.contains(AttrMask::GID)
        && let Some(gid) = v.gid
    {
        a.gid = gid;
        a.returned.insert(AttrMask::GID);
    }
    if a.masks.contains(AttrMask::RDEV)
        && let Some(rdev) = v.rdev
    {
        a.rdev = rdev;
        a.returned.insert(AttrMask::RDEV);
    }
    if a.masks.contains(AttrMask::BLOCKS)
        && let Some(blocks) = v.blocks
    {
        // `v.blocks` is already FATTR4_SPACE_USED converted to 512-byte
        // units in `parse_attr_list` (matching the dummy's `st_blocks`).
        a.blocks = blocks;
        a.returned.insert(AttrMask::BLOCKS);
    }
    if a.masks.contains(AttrMask::MTIME)
        && let Some((s, n)) = v.mtime
    {
        a.mtime_sec = s;
        a.mtime_nsec = n;
        a.returned.insert(AttrMask::MTIME);
    }
    if a.masks.contains(AttrMask::ATIME)
        && let Some((s, n)) = v.atime
    {
        a.atime_sec = s;
        a.atime_nsec = n;
        a.returned.insert(AttrMask::ATIME);
    }
    if a.masks.contains(AttrMask::CTIME)
        && let Some((s, n)) = v.ctime
    {
        a.ctime_sec = s;
        a.ctime_nsec = n;
        a.returned.insert(AttrMask::CTIME);
    }
    if a.masks.contains(AttrMask::NAMED_ATTR)
        && let Some(has) = v.has_named_attr
    {
        a.has_named_attr = has;
        a.returned.insert(AttrMask::NAMED_ATTR);
    }
}

/// Whether a single `FATTR4_TYPE` attribute payload names a symlink. A
/// malformed list is an error, never an implicit "not a symlink".
fn type_is_symlink(attrs: &[u8]) -> VfResult<bool> {
    let mut off = 0;
    Ok(read_u32(attrs, &mut off)? == nfs_ftype4_NF4LNK)
}

fn read_u32(buf: &[u8], off: &mut usize) -> VfResult<u32> {
    let end = off.checked_add(4).ok_or_else(attr_decode_error)?;
    let bytes = buf.get(*off..end).ok_or_else(attr_decode_error)?;
    let v = u32::from_be_bytes(bytes.try_into().expect("four-byte slice"));
    *off = end;
    Ok(v)
}

fn read_u64(buf: &[u8], off: &mut usize) -> VfResult<u64> {
    let end = off.checked_add(8).ok_or_else(attr_decode_error)?;
    let bytes = buf.get(*off..end).ok_or_else(attr_decode_error)?;
    let v = u64::from_be_bytes(bytes.try_into().expect("eight-byte slice"));
    *off = end;
    Ok(v)
}

fn attr_decode_error() -> VfError {
    VfError::transport(0, "malformed NFS attribute list")
}

/// Read an XDR `nfstime4`: `int64 seconds; uint32 nseconds` (12 bytes).
fn read_nfstime(buf: &[u8], off: &mut usize) -> VfResult<(i64, u32)> {
    let end = off.checked_add(12).ok_or_else(attr_decode_error)?;
    let bytes = buf.get(*off..end).ok_or_else(attr_decode_error)?;
    let secs = i64::from_be_bytes(bytes[..8].try_into().expect("eight-byte slice"));
    let nsec = u32::from_be_bytes(bytes[8..].try_into().expect("four-byte slice"));
    *off = end;
    Ok((secs, nsec))
}

/// Read an XDR `utf8string`: length + padded bytes.
fn read_str(buf: &[u8], off: &mut usize) -> VfResult<Vec<u8>> {
    let len = read_u32(buf, off)? as usize;
    let padded = len.checked_add(3).ok_or_else(attr_decode_error)? & !3;
    let end = off.checked_add(padded).ok_or_else(attr_decode_error)?;
    let data_end = off.checked_add(len).ok_or_else(attr_decode_error)?;
    if end > buf.len() {
        return Err(attr_decode_error());
    }
    let s = buf[*off..data_end].to_vec();
    *off = end;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_path_is_namespace_relative_and_server_path_prepends_the_export_root() {
        let root = Path::new("/export/data");
        // Absolute application paths are relative to the export namespace,
        // not the server path (the bug fixed here included the export root).
        assert_eq!(
            namespace_path(Path::new(""), Path::new("/foo")),
            PathBuf::from("foo")
        );
        assert_eq!(
            server_path_for(root, Path::new(""), Path::new("/foo")),
            PathBuf::from("/export/data/foo")
        );
        // Relative paths resolve against the namespace-relative cwd.
        assert_eq!(
            namespace_path(Path::new("sub"), Path::new("f")),
            PathBuf::from("sub/f")
        );
        assert_eq!(
            server_path_for(root, Path::new("sub"), Path::new("f")),
            PathBuf::from("/export/data/sub/f")
        );
        // The application root itself is empty relative to the namespace and
        // maps to the export root when resolved on the server.
        assert_eq!(
            namespace_path(Path::new(""), Path::new("/")),
            PathBuf::new()
        );
        assert_eq!(
            server_path_for(root, Path::new(""), Path::new("/")),
            PathBuf::from("/export/data")
        );
        // `..` cannot escape the namespace root.
        assert_eq!(
            namespace_path(Path::new(""), Path::new("../../etc")),
            PathBuf::from("etc")
        );
    }

    #[test]
    fn malformed_type_attribute_is_an_error_not_a_regular_type() {
        // An empty or truncated FATTR4_TYPE payload must not be silently
        // treated as "not a symlink" (which would skip following a link).
        assert!(type_is_symlink(&[]).is_err());
        assert!(type_is_symlink(&[0, 0, 0]).is_err());

        assert!(type_is_symlink(&nfs_ftype4_NF4LNK.to_be_bytes()).unwrap());
        assert!(!type_is_symlink(&nfs_ftype4_NF4REG.to_be_bytes()).unwrap());
    }

    #[test]
    fn descriptor_chunk_errors_report_the_original_vector_owner() {
        // The first caller was split into two wire chunks; chunk 1 still
        // belongs to caller 0, rather than caller 1.
        let owners = [0, 0, 1];
        let error = remap_descriptor_chunk_error(VfError::failure(1, ERR_EBADF), 0, &owners);
        assert_eq!(error.index_opt(), Some(0));
    }

    #[test]
    fn read_all_round_remaps_failures_and_rejects_zero_progress() {
        let active = [2, 5];
        let remapped = remap_active_error(VfError::failure(0, ERR_IO), &active);
        assert_eq!(remapped.index_opt(), Some(2));
        let remapped = remap_active_error(VfError::failure(1, ERR_IO), &active);
        assert_eq!(remapped.index_opt(), Some(5));

        let mut out = vec![Vec::new(); 6];
        let mut offsets = vec![0; 6];
        let stalled = [ReadResult {
            file: VfFile::from_path("/f"),
            offset: 7,
            data: Vec::new(),
            eof: false,
        }];
        let error =
            merge_read_allv_round(&[5], &stalled, &mut out, &mut offsets, &mut 0, usize::MAX)
                .unwrap_err();
        assert!(error.is_transport());
        assert_eq!(error.index_opt(), Some(5));
    }

    #[test]
    fn read_all_round_preserves_original_positions_after_cohort_shrinks() {
        let mut out = vec![Vec::new(); 3];
        let mut offsets = vec![0; 3];
        let results = [
            ReadResult {
                file: VfFile::from_path("/one"),
                offset: 0,
                data: b"a".to_vec(),
                eof: true,
            },
            ReadResult {
                file: VfFile::from_path("/two"),
                offset: 4,
                data: b"bc".to_vec(),
                eof: false,
            },
        ];
        let next = merge_read_allv_round(
            &[1, 2],
            &results,
            &mut out,
            &mut offsets,
            &mut 0,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(next, [2]);
        assert_eq!(out[1], b"a");
        assert_eq!(out[2], b"bc");
        assert_eq!(offsets[2], 6);
    }

    #[test]
    fn read_all_batches_never_request_beyond_the_remaining_allocation_budget() {
        let active = [0, 1, 2, 3];
        let (cohort, window) = bounded_read_allv_batch(&active, 3, 1 << 20, 1 << 20);
        assert_eq!(cohort, [0, 1, 2]);
        assert!(cohort.len() * window <= 3);

        let (cohort, window) = bounded_read_allv_batch(&active, 0, 1 << 20, 1 << 20);
        assert_eq!(cohort, [0]);
        assert_eq!(window, 1);
    }

    #[test]
    fn walk_page_decoder_stops_at_entry_and_path_budgets() {
        let page = [
            crate::client::DirEntry {
                name: b"one".to_vec(),
                cookie: 1,
                attrs: Vec::new(),
            },
            crate::client::DirEntry {
                name: b"two".to_vec(),
                cookie: 0,
                attrs: Vec::new(),
            },
        ];
        let mut count = 0;
        let mut bytes = 0;
        let mut output = Vec::new();
        let error = append_bounded_walk_page(
            Path::new("/root"),
            Path::new("/root"),
            AttrMask::empty(),
            &[],
            &page,
            WalkOptions::new().max_entries(1),
            &mut count,
            &mut bytes,
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error.err_no(), libc::EFBIG as u32);
        assert_eq!(count, 1);
        assert_eq!(output.len(), 1);

        count = 0;
        bytes = 0;
        output.clear();
        let error = append_bounded_walk_page(
            Path::new("/root"),
            Path::new("/root"),
            AttrMask::empty(),
            &[],
            &page[..1],
            WalkOptions::new().max_path_bytes(1),
            &mut count,
            &mut bytes,
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error.err_no(), libc::EFBIG as u32);
        assert_eq!(count, 0);
        assert!(output.is_empty());
    }

    #[test]
    fn malformed_readdir_attributes_are_transport_errors() {
        let error = parse_attr_list(&[FATTR4_SIZE], &[0, 0, 0]).unwrap_err();
        assert!(error.is_transport());

        // XDR strings occupy a four-byte-aligned field. A payload without
        // its required padding must not be accepted at the end of an attrlist.
        let mut unpadded = 1u32.to_be_bytes().to_vec();
        unpadded.push(b'x');
        let error = parse_attr_list(&[FATTR4_OWNER], &unpadded).unwrap_err();
        assert!(error.is_transport());

        let mut trailing = 7u64.to_be_bytes().to_vec();
        trailing.push(0);
        let error = parse_attr_list(&[FATTR4_SIZE], &trailing).unwrap_err();
        assert!(error.is_transport());
    }

    #[test]
    fn adb_offset_overflow_preserves_request_index() {
        let pattern = Adb::blocknum_only("/overflow", u64::MAX, 2, 2, 0, 0);
        let error = adb_block_base(&pattern, 1, 4).unwrap_err();
        assert_eq!(error.index_opt(), Some(4));
        assert_eq!(error.err_no(), libc::EOVERFLOW as u32);
        let error = adb_field_offset(u64::MAX, 1, 5).unwrap_err();
        assert_eq!(error.index_opt(), Some(5));
    }

    #[test]
    fn recovery_only_retries_transport_and_recoverable_session_statuses() {
        assert!(NfsVecFs::needs_recovery(&VfError::transport(None, "reset")));
        for status in [
            nfsstat4_NFS4ERR_EXPIRED,
            nfsstat4_NFS4ERR_GRACE,
            nfsstat4_NFS4ERR_STALE_CLIENTID,
            nfsstat4_NFS4ERR_STALE_STATEID,
            nfsstat4_NFS4ERR_BAD_STATEID,
            nfsstat4_NFS4ERR_BADSESSION,
            nfsstat4_NFS4ERR_DEADSESSION,
        ] {
            assert!(NfsVecFs::needs_recovery(&VfError::failure(0, status)));
        }
        assert!(!NfsVecFs::needs_recovery(&VfError::failure(
            0,
            nfsstat4_NFS4ERR_NOENT,
        )));
    }

    #[test]
    fn recovery_reopen_flags_cannot_repeat_creation_or_truncation() {
        let original = libc::O_RDWR | libc::O_APPEND | libc::O_CREAT | libc::O_EXCL | libc::O_TRUNC;
        let reopened = non_destructive_reopen_flags(original);
        assert_eq!(reopened & (libc::O_CREAT | libc::O_EXCL | libc::O_TRUNC), 0);
        assert_ne!(reopened & libc::O_APPEND, 0);
        assert_eq!(reopened & libc::O_ACCMODE, libc::O_RDWR);
    }

    #[test]
    fn default_recovery_window_can_span_a_conventional_server_grace_period() {
        let policy = NfsRecoveryPolicy::default();
        assert!(policy.reconnect_attempts > 1);
        assert!(policy.max_elapsed >= Duration::from_secs(90));
        assert!(policy.initial_backoff <= policy.max_backoff);
    }

    #[test]
    fn connection_options_preserve_auth_sys_as_the_compatible_default() {
        let options = NfsConnectOptions::default();
        assert_eq!(options.minorversion, None);
        assert_eq!(options.connect_timeout, Duration::from_secs(10));
        assert_eq!(options.request_timeout, Duration::from_secs(5));
        assert_eq!(options.authentication, NfsAuthentication::AuthSys);
        assert_eq!(options.root, Path::new("/"));
        assert!(options.auto_reconnect);
        assert_eq!(options.max_compound_bytes, 0);
    }

    #[test]
    fn builder_collects_connection_and_runtime_configuration() {
        let policy = NfsRecoveryPolicy {
            reconnect_attempts: 3,
            ..NfsRecoveryPolicy::default()
        };
        let builder = NfsClientBuilder::new("server:2049")
            .root("/export/app")
            .minor_version(Some(1))
            .client_owner(b"production-client-17".to_vec())
            .request_timeout(Duration::from_secs(7))
            .recovery_policy(policy)
            .auto_reconnect(false)
            .max_compound_bytes(64 * 1024);
        assert_eq!(builder.host, "server:2049");
        assert_eq!(builder.options.root, Path::new("/export/app"));
        assert_eq!(builder.options.minorversion, Some(1));
        assert_eq!(
            builder.options.client_owner.as_deref(),
            Some(b"production-client-17".as_slice())
        );
        assert_eq!(builder.options.request_timeout, Duration::from_secs(7));
        assert_eq!(builder.options.recovery_policy, policy);
        assert!(!builder.options.auto_reconnect);
        assert_eq!(builder.options.max_compound_bytes, 64 * 1024);
    }

    #[test]
    fn secure_authentication_requirement_fails_closed_before_network_io() {
        let error = NfsClientBuilder::new("unreachable.invalid")
            .require_secure_authentication(true)
            .connect()
            .err()
            .expect("AUTH_SYS must be rejected");
        assert_eq!(error.domain(), ErrorDomain::Client);
        assert_eq!(error.err_no(), ERR_ACCES);
        assert_eq!(error.operation(), Some("connect"));
    }

    #[test]
    fn invalid_client_owner_fails_before_network_io() {
        for owner in [Vec::new(), vec![b'x'; 1025]] {
            let error = NfsClientBuilder::new("unreachable.invalid")
                .client_owner(owner)
                .connect()
                .err()
                .expect("invalid client owner must be rejected");
            assert_eq!(error.domain(), ErrorDomain::Client);
            assert_eq!(error.err_no(), ERR_INVAL);
            assert_eq!(error.operation(), Some("connect"));
        }
    }
}
