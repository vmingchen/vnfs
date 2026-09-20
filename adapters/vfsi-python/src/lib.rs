//! Shared Python bindings for vectorized filesystem backends.
//!
//! This module exposes the [`VecFs`] surface as a `NfsClient` PyO3 class
//! to protocol-specific extension crates. Every fsspec bulk operation funnels
//! through vectorized calls here, so round trips scale with batches and
//! directories rather than files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pyo3::exceptions::{
    PyConnectionError, PyFileExistsError, PyFileNotFoundError, PyIsADirectoryError,
    PyNotADirectoryError, PyNotImplementedError, PyOSError, PyPermissionError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyString};

use vfsi_core::{
    AttrMask, ERR_ACCES, ERR_EXIST, ERR_INVAL, ERR_ISDIR, ERR_NOENT, ERR_NOTDIR, ReadOp,
    ReadResult, SeekFrom, VF_CAP_HARDLINKS, VF_CAP_LSTAT, VF_CAP_NON_UTF8_PATHS,
    VF_CAP_POSIX_METADATA, VF_CAP_SERVER_COPY, VF_CAP_SYMLINKS, VF_ERR_UNSUPPORTED, VfAttrs,
    VfError, VfFile, VfOffset, VfType, WriteOp, WriteResult,
};
#[cfg(feature = "dummy")]
use vfsi_local::DummyVecFs;
#[cfg(feature = "nfs-rpcsec-gss")]
use vfsi_nfs::RpcsecGssProtection;
#[cfg(feature = "nfs")]
use vfsi_nfs::compound::{compound_stats, rpc_stats};
#[cfg(feature = "nfs")]
use vfsi_nfs::{NfsAuthentication, NfsClientBuilder};
#[cfg(feature = "smb")]
use vfsi_smb::SmbVecFs;
use vfsi_sync::{ReadAllOptions, VecFs, WalkOptions};

#[cfg(not(feature = "nfs"))]
fn compound_stats() -> (u64, u64, u64, u64) {
    (0, 0, 0, 0)
}

#[cfg(not(feature = "nfs"))]
fn rpc_stats() -> (u64, u64) {
    (0, 0)
}

/// errno keyed by the operation index in the caller's request.
type ErrnoMap = HashMap<usize, u32>;
/// Per-index attribute results plus failures.
type AttrsManyResult = Result<(Vec<Option<VfAttrs>>, ErrnoMap), VfError>;
/// Per-index attribute dicts (None on failure) plus failures.
type StatManyResult = (Vec<Option<Py<PyDict>>>, ErrnoMap);
/// Per-index byte reads (None on failure) plus failures.
type ReadManyResult = (Vec<Option<Vec<u8>>>, ErrnoMap);
/// Per-index copied byte counts (None on failure) plus failures.
type CopyManyResult = (Vec<Option<u64>>, ErrnoMap);
/// Directory paths and their entries returned to Python by a tree walk.
type WalkResult = Vec<(Py<PyString>, Vec<Py<PyDict>>)>;

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn lock_err<T>(_: std::sync::PoisonError<T>) -> PyErr {
    PyErr::new::<PyOSError, _>("vnfs client lock poisoned")
}

/// Map a `VfError` onto the Python exception class that matches its errno.
fn to_py_err(e: VfError, path: Option<&Path>) -> PyErr {
    let index = e.index_opt();
    let what = path
        .map(|p| format!(": '{}'", p.display()))
        .unwrap_or_default();
    let error = match e {
        VfError::Transport { message, .. } => {
            PyErr::new::<PyConnectionError, _>(format!("{}{}", message, what))
        }
        VfError::Op { err_no, .. } => match err_no {
            ERR_NOENT => PyErr::new::<PyFileNotFoundError, _>((
                2,
                format!("No such file or directory{}", what),
            )),
            ERR_ACCES => {
                PyErr::new::<PyPermissionError, _>((13, format!("Permission denied{}", what)))
            }
            ERR_EXIST => PyErr::new::<PyFileExistsError, _>((17, format!("File exists{}", what))),
            ERR_NOTDIR => {
                PyErr::new::<PyNotADirectoryError, _>((20, format!("Not a directory{}", what)))
            }
            ERR_ISDIR => {
                PyErr::new::<PyIsADirectoryError, _>((21, format!("Is a directory{}", what)))
            }
            ERR_INVAL => PyErr::new::<PyValueError, _>(format!("Invalid argument{}", what)),
            VF_ERR_UNSUPPORTED => {
                PyErr::new::<PyNotImplementedError, _>(format!("operation is unsupported{}", what))
            }
            other => PyErr::new::<PyOSError, _>((other, format!("op failed{}", what))),
        },
        _ => PyErr::new::<PyOSError, _>("unknown vnfs error"),
    };
    if let Some(index) = index {
        Python::attach(|py| {
            let _ = error.value(py).setattr("index", index);
        });
    }
    error
}

/// Attach the failing operation's path to an error from a batched call.
fn map_err_with_path(e: VfError, paths: &[PathBuf]) -> PyErr {
    let path = e
        .index_opt()
        .and_then(|index| paths.get(index))
        .map(PathBuf::as_path);
    to_py_err(e, path)
}

fn contract_error(operation: &str, detail: impl std::fmt::Display) -> PyErr {
    PyOSError::new_err(format!("{operation}: backend contract violation: {detail}"))
}

fn one_read_result(operation: &str, op: &ReadOp, results: Vec<ReadResult>) -> PyResult<ReadResult> {
    let mut results = validate_read_results(operation, std::slice::from_ref(op), results)?;
    results
        .pop()
        .ok_or_else(|| contract_error(operation, "result disappeared after validation"))
}

fn validate_read_results(
    operation: &str,
    ops: &[ReadOp],
    results: Vec<ReadResult>,
) -> PyResult<Vec<ReadResult>> {
    if results.len() != ops.len() {
        return Err(contract_error(
            operation,
            format!(
                "expected {} results, received {}",
                ops.len(),
                results.len()
            ),
        ));
    }
    for (index, (op, result)) in ops.iter().zip(&results).enumerate() {
        if result.file != op.file {
            return Err(contract_error(
                operation,
                format!("result {index} file does not match request"),
            ));
        }
        if let VfOffset::At(offset) = op.offset
            && result.offset != offset
        {
            return Err(contract_error(
                operation,
                format!(
                    "result {index} offset {} does not match request {offset}",
                    result.offset
                ),
            ));
        }
        if result.data.len() > op.length {
            return Err(contract_error(
                operation,
                format!(
                    "result {index} returned {} bytes for a {}-byte request",
                    result.data.len(),
                    op.length
                ),
            ));
        }
    }
    Ok(results)
}

fn one_write_result(
    operation: &str,
    op: &WriteOp,
    results: Vec<WriteResult>,
) -> PyResult<WriteResult> {
    let mut results = validate_write_results(operation, std::slice::from_ref(op), results)?;
    results
        .pop()
        .ok_or_else(|| contract_error(operation, "result disappeared after validation"))
}

fn validate_write_results(
    operation: &str,
    ops: &[WriteOp],
    results: Vec<WriteResult>,
) -> PyResult<Vec<WriteResult>> {
    if results.len() != ops.len() {
        return Err(contract_error(
            operation,
            format!(
                "expected {} results, received {}",
                ops.len(),
                results.len()
            ),
        ));
    }
    for (index, (op, result)) in ops.iter().zip(&results).enumerate() {
        if result.file != op.file {
            return Err(contract_error(
                operation,
                format!("result {index} file does not match request"),
            ));
        }
        if let VfOffset::At(offset) = op.offset
            && result.offset != offset
        {
            return Err(contract_error(
                operation,
                format!(
                    "result {index} offset {} does not match request {offset}",
                    result.offset
                ),
            ));
        }
        if result.written > op.data.len() {
            return Err(contract_error(
                operation,
                format!(
                    "result {index} reported {} bytes for a {}-byte request",
                    result.written,
                    op.data.len()
                ),
            ));
        }
    }
    Ok(results)
}

fn descriptor(operation: &str, file: &VfFile) -> PyResult<i64> {
    file.fd()
        .map(i64::from)
        .ok_or_else(|| contract_error(operation, "backend did not return a descriptor"))
}

// ---------------------------------------------------------------------------
// Attribute conversion
// ---------------------------------------------------------------------------

/// All attributes the Python layer wants for `ls`/`info` (skip NAMED_ATTR:
/// it costs the server a per-entry xattr enumeration).
fn full_mask() -> AttrMask {
    AttrMask::MODE
        .union(AttrMask::SIZE)
        .union(AttrMask::NLINK)
        .union(AttrMask::FILEID)
        .union(AttrMask::BLOCKS)
        .union(AttrMask::UID)
        .union(AttrMask::GID)
        .union(AttrMask::ATIME)
        .union(AttrMask::MTIME)
        .union(AttrMask::CTIME)
        .union(AttrMask::CHANGE)
}

fn attrs_to_dict(py: Python<'_>, a: &VfAttrs) -> PyResult<Py<PyDict>> {
    let d = PyDict::new(py);
    let r = a.returned;
    let name: Py<PyString> = match a.file.path() {
        Some(p) => p.as_os_str().into_pyobject(py)?.unbind(),
        None => "".into_pyobject(py)?.unbind(),
    };
    d.set_item("name", name)?;
    let ftype = match a.ftype {
        VfType::Regular => "file",
        VfType::Directory => "directory",
        VfType::Symlink => "symlink",
        VfType::BlockDevice => "block",
        VfType::CharDevice => "char",
        VfType::Fifo => "fifo",
        VfType::Socket => "socket",
        VfType::Other(_) => "other",
    };
    d.set_item("type", ftype)?;
    d.set_item("islink", matches!(a.ftype, VfType::Symlink))?;
    if r.contains(AttrMask::SIZE) {
        d.set_item("size", a.size)?;
    }
    if r.contains(AttrMask::MODE) {
        d.set_item("mode", a.mode)?;
    }
    if r.contains(AttrMask::UID) {
        d.set_item("uid", a.uid)?;
    }
    if r.contains(AttrMask::GID) {
        d.set_item("gid", a.gid)?;
    }
    if r.contains(AttrMask::NLINK) {
        d.set_item("nlink", a.nlink)?;
    }
    if r.contains(AttrMask::FILEID) {
        d.set_item("fileid", a.fileid)?;
    }
    if r.contains(AttrMask::CHANGE) {
        d.set_item("change", a.change)?;
    }
    if r.contains(AttrMask::BLOCKS) {
        d.set_item("blocks", a.blocks)?;
    }
    if r.contains(AttrMask::CTIME) {
        d.set_item("created", a.ctime_sec)?;
        d.set_item(
            "created_ns",
            i128::from(a.ctime_sec) * 1_000_000_000 + i128::from(a.ctime_nsec),
        )?;
    }
    if r.contains(AttrMask::MTIME) {
        d.set_item("modified", a.mtime_sec)?;
        d.set_item(
            "modified_ns",
            i128::from(a.mtime_sec) * 1_000_000_000 + i128::from(a.mtime_nsec),
        )?;
    }
    if r.contains(AttrMask::ATIME) {
        d.set_item("accessed", a.atime_sec)?;
        d.set_item(
            "accessed_ns",
            i128::from(a.atime_sec) * 1_000_000_000 + i128::from(a.atime_nsec),
        )?;
    }
    if r.intersects(AttrMask::CHANGE | AttrMask::MTIME) {
        let checksum = if r.contains(AttrMask::CHANGE) {
            a.change
        } else {
            a.fileid
                ^ (a.mtime_sec as u64).rotate_left(17)
                ^ u64::from(a.mtime_nsec).rotate_left(41)
                ^ a.size.rotate_left(7)
        };
        d.set_item("checksum", checksum)?;
    }
    Ok(d.into())
}

// ---------------------------------------------------------------------------
// Batched attribute helpers (one compound per batch, retrying dropped
// per-operation failures)
// ---------------------------------------------------------------------------

/// Fetch attrs for `paths` in batches, returning per-path results. A failed
/// operation is recorded in the errno map (keyed by the original index) and
/// the remaining operations are retried in fresh batches. Transport failures
/// abort with `Err`.
fn attrs_many_impl(
    fs: &mut dyn VecFs,
    paths: &[PathBuf],
    masks: AttrMask,
    follow: bool,
) -> AttrsManyResult {
    let mut remaining: Vec<usize> = (0..paths.len()).collect();
    let mut results: Vec<Option<VfAttrs>> = vec![None; paths.len()];
    let mut errors: HashMap<usize, u32> = HashMap::new();
    while !remaining.is_empty() {
        let mut attrs: Vec<VfAttrs> = remaining
            .iter()
            .map(|&i| VfAttrs {
                file: VfFile::from_os_path(&paths[i]),
                masks,
                ..VfAttrs::default()
            })
            .collect();
        let res = if follow {
            fs.getattrsv(&mut attrs)
        } else {
            fs.lgetattrsv(&mut attrs)
        };
        match res {
            Ok(()) => {
                for (&i, a) in remaining.iter().zip(attrs) {
                    results[i] = Some(a);
                }
                break;
            }
            Err(e) => {
                let Some(bi) = e.index_opt() else {
                    return Err(e);
                };
                if bi >= remaining.len() {
                    return Err(e);
                }
                let orig = remaining[bi];
                errors.insert(orig, e.err_no());
                remaining.remove(bi);
            }
        }
    }
    Ok((results, errors))
}

/// Read every file in full (offset 0 to EOF) in batched, no-stat reads.
/// Returns per-path bytes (None on failure) and an errno map.
fn read_allv_impl(
    fs: &mut dyn VecFs,
    paths: &[PathBuf],
    max_total_bytes: usize,
) -> Result<ReadManyResult, VfError> {
    let files: Vec<VfFile> = paths.iter().map(|p| VfFile::from_os_path(p)).collect();
    let mut results: Vec<Option<Vec<u8>>> = vec![None; paths.len()];
    let mut errors: ErrnoMap = HashMap::new();
    let mut remaining: Vec<usize> = (0..paths.len()).collect();
    while !remaining.is_empty() {
        let subset: Vec<VfFile> = remaining.iter().map(|&i| files[i].clone()).collect();
        match fs.read_allv_with_options(
            &subset,
            ReadAllOptions::new().max_total_bytes(max_total_bytes),
        ) {
            Ok(bufs) => {
                for (&i, buf) in remaining.iter().zip(bufs) {
                    results[i] = Some(buf);
                }
                break;
            }
            Err(e) => {
                let Some(bi) = e.index_opt() else {
                    return Err(e);
                };
                if bi >= remaining.len() {
                    return Err(e);
                }
                let orig = remaining[bi];
                errors.insert(orig, e.err_no());
                remaining.remove(bi);
            }
        }
    }
    Ok((results, errors))
}

fn validate_directory_results(
    entries: &[VfAttrs],
    max_entries: usize,
    max_path_bytes: usize,
) -> Result<(), VfError> {
    if entries.len() > max_entries {
        return Err(VfError::failure(max_entries, libc::EFBIG as u32));
    }
    let mut path_bytes = 0usize;
    for (index, entry) in entries.iter().enumerate() {
        let bytes = entry.file.path().map_or(0, |path| path.as_os_str().len());
        path_bytes = path_bytes
            .checked_add(bytes)
            .ok_or_else(|| VfError::failure(index, libc::EFBIG as u32))?;
        if path_bytes > max_path_bytes {
            return Err(VfError::failure(index, libc::EFBIG as u32));
        }
    }
    Ok(())
}

fn mode_to_flags(mode: &str) -> PyResult<i32> {
    use libc::{O_APPEND, O_CREAT, O_EXCL, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};
    let base: String = mode.chars().filter(|c| *c != 'b' && *c != 't').collect();
    match base.as_str() {
        "r" => Ok(O_RDONLY),
        "r+" => Ok(O_RDWR),
        "w" => Ok(O_WRONLY | O_CREAT | O_TRUNC),
        "w+" => Ok(O_RDWR | O_CREAT | O_TRUNC),
        "a" => Ok(O_WRONLY | O_CREAT | O_APPEND),
        "a+" => Ok(O_RDWR | O_CREAT | O_APPEND),
        "x" => Ok(O_WRONLY | O_CREAT | O_EXCL),
        "x+" => Ok(O_RDWR | O_CREAT | O_EXCL),
        _ => Err(PyValueError::new_err(format!(
            "unsupported file mode: {:?}",
            mode
        ))),
    }
}

// ---------------------------------------------------------------------------
// The PyO3 client
// ---------------------------------------------------------------------------

/// One vectorized filesystem client behind a mutex.
#[pyclass]
struct NfsClient {
    fs: Mutex<Option<Box<dyn VecFs + Send>>>,
    read_all_max_total_bytes: usize,
    directory_max_entries: usize,
    directory_max_path_bytes: usize,
    walk_max_depth: usize,
}

impl NfsClient {
    /// Run Rust-only client work without keeping the Python interpreter
    /// attached. This is required for blocking network I/O and also prevents
    /// deadlocks when another Python thread is waiting on the client mutex.
    fn with_fs<T, F>(&self, py: Python<'_>, operation: F) -> PyResult<T>
    where
        T: Send,
        F: FnOnce(&mut (dyn VecFs + Send)) -> PyResult<T> + Send,
    {
        py.detach(|| {
            let mut fs = self.fs.lock().map_err(lock_err)?;
            let fs = fs
                .as_deref_mut()
                .ok_or_else(|| PyValueError::new_err("native client is closed"))?;
            operation(fs)
        })
    }
}

#[pymethods]
impl NfsClient {
    /// Connect to an NFS server (`backend="nfs"`, default), an SMB2/3 share
    /// (`backend="smb"`), or a local directory (`backend="dummy"`).
    #[new]
    #[pyo3(signature = (host, backend="nfs", root=None, compound_size_limit=None, minor_version=None, share=None, username="", password="", domain="", connect_timeout=10.0, request_timeout=5.0, read_all_max_total_bytes=16777216, directory_max_entries=100000, directory_max_path_bytes=16777216, walk_max_depth=128, authentication="auth_sys", service_principal=None, require_secure_authentication=false))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        host: &str,
        backend: &str,
        root: Option<PathBuf>,
        compound_size_limit: Option<usize>,
        minor_version: Option<u32>,
        share: Option<&str>,
        username: &str,
        password: &str,
        domain: &str,
        connect_timeout: f64,
        request_timeout: f64,
        read_all_max_total_bytes: usize,
        directory_max_entries: usize,
        directory_max_path_bytes: usize,
        walk_max_depth: usize,
        authentication: &str,
        service_principal: Option<String>,
        require_secure_authentication: bool,
    ) -> PyResult<Self> {
        let connect_timeout = Duration::try_from_secs_f64(connect_timeout)
            .map_err(|_| PyValueError::new_err("connect_timeout must be finite and positive"))?;
        let request_timeout = Duration::try_from_secs_f64(request_timeout)
            .map_err(|_| PyValueError::new_err("request_timeout must be finite and positive"))?;
        if connect_timeout.is_zero() || request_timeout.is_zero() {
            return Err(PyValueError::new_err("timeouts must be positive"));
        }
        // Each protocol extension enables only its own backend. Keep the
        // stable constructor ABI shared while allowing cfg-disabled arguments.
        let _ = (
            &root,
            compound_size_limit,
            minor_version,
            share,
            username,
            password,
            domain,
            authentication,
            &service_principal,
            require_secure_authentication,
        );
        let fs: Box<dyn VecFs + Send> = py.detach(|| {
            Ok(match backend {
                #[cfg(feature = "nfs")]
                "nfs" => {
                    if minor_version.is_some_and(|version| !matches!(version, 1 | 2)) {
                        return Err(PyValueError::new_err("minor_version must be 1, 2, or None"));
                    }
                    let nfs_authentication = match authentication {
                        "auth_sys" => NfsAuthentication::AuthSys,
                        #[cfg(feature = "nfs-rpcsec-gss")]
                        "krb5" => NfsAuthentication::RpcsecGss {
                            service_principal: service_principal.clone(),
                            protection: RpcsecGssProtection::Authentication,
                        },
                        #[cfg(feature = "nfs-rpcsec-gss")]
                        "krb5i" => NfsAuthentication::RpcsecGss {
                            service_principal: service_principal.clone(),
                            protection: RpcsecGssProtection::Integrity,
                        },
                        #[cfg(not(feature = "nfs-rpcsec-gss"))]
                        "krb5" | "krb5i" => {
                            return Err(PyNotImplementedError::new_err(
                                "this extension was built without RPCSEC_GSS support",
                            ));
                        }
                        other => {
                            return Err(PyValueError::new_err(format!(
                                "authentication must be 'auth_sys', 'krb5', or 'krb5i', got {other:?}"
                            )));
                        }
                    };
                    let mut builder = NfsClientBuilder::new(host)
                        .minor_version(minor_version)
                        .connect_timeout(connect_timeout)
                        .request_timeout(request_timeout)
                        .authentication(nfs_authentication)
                        .require_secure_authentication(require_secure_authentication);
                    if let Some(limit) = compound_size_limit {
                        builder = builder.max_compound_bytes(limit);
                    }
                    let nfs = builder
                        .connect()
                        .map_err(|e| to_py_err(e, Some(Path::new(host))))?;
                    Box::new(nfs) as Box<dyn VecFs + Send>
                }
                #[cfg(feature = "smb")]
                "smb" => {
                    if compound_size_limit.is_some() || minor_version.is_some() {
                        return Err(PyValueError::new_err(
                            "compound_size_limit and minor_version are NFS-only",
                        ));
                    }
                    let share = share.filter(|share| !share.is_empty()).ok_or_else(|| {
                        PyValueError::new_err("share is required for backend=\"smb\"")
                    })?;
                    Box::new(
                        SmbVecFs::connect(host, share, username, password, domain)
                            .map_err(|e| to_py_err(e, Some(Path::new(host))))?,
                    ) as Box<dyn VecFs + Send>
                }
                #[cfg(feature = "dummy")]
                "dummy" => {
                    let root_path = match root {
                        Some(r) => r,
                        None => std::env::temp_dir().join(format!(
                            "vfsi_dummy_{}_{}",
                            std::process::id(),
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos())
                                .unwrap_or(0)
                        )),
                    };
                    if root_path.exists() && !root_path.is_dir() {
                        return Err(PyValueError::new_err(format!(
                            "dummy root exists and is not a directory: {}",
                            root_path.display()
                        )));
                    }
                    std::fs::create_dir_all(&root_path)
                        .map_err(|e| PyOSError::new_err(format!("create dummy root: {}", e)))?;
                    Box::new(
                        DummyVecFs::try_new(root_path)
                            .map_err(|error| PyOSError::new_err(error.to_string()))?,
                    ) as Box<dyn VecFs + Send>
                }
                other => {
                    return Err(PyValueError::new_err(format!(
                        "unknown backend: {:?}",
                        other
                    )));
                }
            })
        })?;
        Ok(NfsClient {
            fs: Mutex::new(Some(fs)),
            read_all_max_total_bytes,
            directory_max_entries,
            directory_max_path_bytes,
            walk_max_depth,
        })
    }

    /// Release all descriptors and the network session without holding the
    /// Python interpreter lock.
    fn shutdown(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            let fs = self.fs.lock().map_err(lock_err)?.take();
            drop(fs);
            Ok(())
        })
    }

    /// Forget an inherited session in a forked child without issuing any
    /// protocol cleanup on the parent's connection. The operating system
    /// still closes the child's duplicated descriptors when it exits.
    fn _abandon_after_fork(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            if let Some(fs) = self.fs.lock().map_err(lock_err)?.take() {
                std::mem::forget(fs);
            }
            Ok(())
        })
    }

    /// Negotiated NFS minor version, or None for the dummy backend.
    // The erased multi-protocol VecFs object cannot use a concrete backend's
    // extension trait; this compatibility query is intentional at this seam.
    #[allow(deprecated)]
    fn minor_version(&self, py: Python<'_>) -> PyResult<Option<u32>> {
        self.with_fs(py, |fs| Ok(fs.nfs_minorversion()))
    }

    /// Negotiated SMB dialect revision, or None for non-SMB backends.
    #[allow(deprecated)]
    fn smb_dialect(&self, py: Python<'_>) -> PyResult<Option<u16>> {
        self.with_fs(py, |fs| Ok(fs.smb_dialect()))
    }

    /// Current backend capability bitset (see CAP_SERVER_COPY).
    fn capabilities(&self, py: Python<'_>) -> PyResult<u64> {
        self.with_fs(py, |fs| Ok(fs.capabilities()))
    }

    /// Whether NFSv4.2 server COPY is currently enabled.
    fn server_copy_enabled(&self, py: Python<'_>) -> PyResult<bool> {
        self.with_fs(py, |fs| Ok(fs.capabilities() & VF_CAP_SERVER_COPY != 0))
    }

    // -- single-op ------------------------------------------------------------------

    /// Stat `path` (follows symlinks), returning an attribute dict.
    fn stat(&self, py: Python<'_>, path: PathBuf) -> PyResult<Py<PyDict>> {
        let mut a = VfAttrs {
            file: VfFile::from_os_path(&path),
            masks: full_mask(),
            ..VfAttrs::default()
        };
        let a = self.with_fs(py, move |fs| {
            fs.getattrsv(std::slice::from_mut(&mut a))
                .map_err(|e| to_py_err(e, Some(path.as_path())))?;
            Ok(a)
        })?;
        attrs_to_dict(py, &a)
    }

    /// lstat `path` (does not follow symlinks).
    fn lstat(&self, py: Python<'_>, path: PathBuf) -> PyResult<Py<PyDict>> {
        let mut a = VfAttrs {
            file: VfFile::from_os_path(&path),
            masks: full_mask(),
            ..VfAttrs::default()
        };
        let a = self.with_fs(py, move |fs| {
            fs.lgetattrsv(std::slice::from_mut(&mut a))
                .map_err(|e| to_py_err(e, Some(path.as_path())))?;
            Ok(a)
        })?;
        attrs_to_dict(py, &a)
    }

    fn exists(&self, py: Python<'_>, path: PathBuf) -> PyResult<bool> {
        Ok(self.exists_many(py, vec![path.clone()])?[0])
    }

    /// Open a file; returns the backend descriptor (an int).
    fn open(&self, py: Python<'_>, path: PathBuf, mode: &str) -> PyResult<i64> {
        let flags = mode_to_flags(mode)?;
        self.with_fs(py, move |fs| {
            let f = fs
                .open(&path, flags, 0o644)
                .map_err(|e| to_py_err(e, Some(path.as_path())))?;
            descriptor("open", &f)
        })
    }

    fn close(&self, py: Python<'_>, fd: i64) -> PyResult<()> {
        self.with_fs(py, move |fs| {
            fs.close(&VfFile::from_fd(fd as i32))
                .map_err(|e| to_py_err(e, None))
        })
    }

    /// Read `length` bytes at the descriptor's current position (advances it).
    fn read(&self, py: Python<'_>, fd: i64, length: usize) -> PyResult<Vec<u8>> {
        self.with_fs(py, move |fs| {
            let op = ReadOp::new(VfFile::from_fd(fd as i32), VfOffset::Cur, length);
            let r = fs
                .readv(std::slice::from_ref(&op))
                .map_err(|e| to_py_err(e, None))?;
            Ok(one_read_result("read", &op, r)?.data)
        })
    }

    /// Write `data` at the descriptor's current position (advances it).
    fn write(&self, py: Python<'_>, fd: i64, data: Vec<u8>) -> PyResult<usize> {
        self.with_fs(py, move |fs| {
            let op = WriteOp::new(VfFile::from_fd(fd as i32), VfOffset::Cur, data);
            let w = fs
                .writev(std::slice::from_ref(&op))
                .map_err(|e| to_py_err(e, None))?;
            Ok(one_write_result("write", &op, w)?.written)
        })
    }

    /// Write at the descriptor's current position and return
    /// `(written, resulting_position)`. For O_APPEND descriptors the backend
    /// reports the actual EOF offset selected atomically for this write.
    fn write_positioned(&self, py: Python<'_>, fd: i64, data: Vec<u8>) -> PyResult<(usize, u64)> {
        self.with_fs(py, move |fs| {
            let op = WriteOp::new(VfFile::from_fd(fd as i32), VfOffset::Cur, data);
            let w = fs
                .writev(std::slice::from_ref(&op))
                .map_err(|e| to_py_err(e, None))?;
            let w = one_write_result("write_positioned", &op, w)?;
            let position = w
                .offset
                .checked_add(w.written as u64)
                .ok_or_else(|| PyOSError::new_err("file position overflow"))?;
            Ok((w.written, position))
        })
    }

    /// Read `length` bytes at an absolute offset (does not move the position).
    fn pread(&self, py: Python<'_>, fd: i64, length: usize, offset: u64) -> PyResult<Vec<u8>> {
        self.with_fs(py, move |fs| {
            let op = ReadOp::new(VfFile::from_fd(fd as i32), VfOffset::At(offset), length);
            let r = fs
                .readv(std::slice::from_ref(&op))
                .map_err(|e| to_py_err(e, None))?;
            Ok(one_read_result("pread", &op, r)?.data)
        })
    }

    /// Write `data` at an absolute offset.
    fn pwrite(&self, py: Python<'_>, fd: i64, data: Vec<u8>, offset: u64) -> PyResult<usize> {
        self.with_fs(py, move |fs| {
            let op = WriteOp::new(VfFile::from_fd(fd as i32), VfOffset::At(offset), data);
            let w = fs
                .writev(std::slice::from_ref(&op))
                .map_err(|e| to_py_err(e, None))?;
            Ok(one_write_result("pwrite", &op, w)?.written)
        })
    }

    /// `fseek`; `whence` is 0=SET, 1=CUR, 2=END. Returns the new position.
    fn fseek(&self, py: Python<'_>, fd: i64, offset: i64, whence: i32) -> PyResult<i64> {
        let whence = match whence {
            0 => SeekFrom::Set,
            1 => SeekFrom::Cur,
            2 => SeekFrom::End,
            _ => return Err(PyValueError::new_err("whence must be 0, 1 or 2")),
        };
        self.with_fs(py, move |fs| {
            fs.fseek(&VfFile::from_fd(fd as i32), offset, whence)
                .map_err(|e| to_py_err(e, None))
        })
    }

    fn fstat(&self, py: Python<'_>, fd: i64) -> PyResult<Py<PyDict>> {
        let mut a = VfAttrs {
            file: VfFile::from_fd(fd as i32),
            masks: full_mask(),
            ..VfAttrs::default()
        };
        let a = self.with_fs(py, move |fs| {
            fs.getattrsv(std::slice::from_mut(&mut a))
                .map_err(|e| to_py_err(e, None))?;
            Ok(a)
        })?;
        attrs_to_dict(py, &a)
    }

    /// Fetch attributes for open descriptors in one vector operation.
    fn fstat_many(&self, py: Python<'_>, fds: Vec<i64>) -> PyResult<Vec<Py<PyDict>>> {
        let mut attrs: Vec<VfAttrs> = fds
            .into_iter()
            .map(|fd| VfAttrs {
                file: VfFile::from_fd(fd as i32),
                masks: full_mask(),
                ..VfAttrs::default()
            })
            .collect();
        let attrs = self.with_fs(py, move |fs| {
            fs.getattrsv(&mut attrs).map_err(|e| to_py_err(e, None))?;
            Ok(attrs)
        })?;
        attrs.iter().map(|attrs| attrs_to_dict(py, attrs)).collect()
    }

    /// Read absolute ranges from open descriptors in a vector operation.
    /// Semantic failures are returned per index so healthy reads can still
    /// share compounds; transport failures fail the whole call.
    fn pread_many(
        &self,
        py: Python<'_>,
        fds: Vec<i64>,
        offsets: Vec<u64>,
        lengths: Vec<usize>,
    ) -> PyResult<ReadManyResult> {
        if fds.len() != offsets.len() || fds.len() != lengths.len() {
            return Err(PyValueError::new_err(
                "fds, offsets, and lengths must have equal lengths",
            ));
        }
        self.with_fs(py, move |fs| {
            let mut results: Vec<Option<Vec<u8>>> = vec![None; fds.len()];
            let mut errors = HashMap::new();
            let mut remaining: Vec<usize> = (0..fds.len()).collect();
            while !remaining.is_empty() {
                let ops: Vec<ReadOp> = remaining
                    .iter()
                    .map(|&index| {
                        ReadOp::new(
                            VfFile::from_fd(fds[index] as i32),
                            VfOffset::At(offsets[index]),
                            lengths[index],
                        )
                    })
                    .collect();
                match fs.readv(&ops) {
                    Ok(reads) => {
                        let reads = validate_read_results("pread_many", &ops, reads)?;
                        for (&index, read) in remaining.iter().zip(reads) {
                            results[index] = Some(read.data);
                        }
                        break;
                    }
                    Err(error) => {
                        if error.is_transport() {
                            return Err(to_py_err(error, None));
                        }
                        let Some(batch_index) = error.index_opt() else {
                            return Err(to_py_err(error, None));
                        };
                        if batch_index >= remaining.len() {
                            return Err(to_py_err(error, None));
                        }
                        let original_index = remaining.remove(batch_index);
                        errors.insert(original_index, error.err_no());
                    }
                }
            }
            Ok((results, errors))
        })
    }

    /// Write absolute ranges to open descriptors in one vector operation.
    fn pwrite_many(
        &self,
        py: Python<'_>,
        fds: Vec<i64>,
        offsets: Vec<u64>,
        datas: Vec<Vec<u8>>,
    ) -> PyResult<Vec<usize>> {
        if fds.len() != offsets.len() || fds.len() != datas.len() {
            return Err(PyValueError::new_err(
                "fds, offsets, and datas must have equal lengths",
            ));
        }
        let ops: Vec<WriteOp> = fds
            .into_iter()
            .zip(offsets)
            .zip(datas)
            .map(|((fd, offset), data)| {
                WriteOp::new(VfFile::from_fd(fd as i32), VfOffset::At(offset), data)
            })
            .collect();
        self.with_fs(py, move |fs| {
            let writes = fs.writev(&ops).map_err(|e| to_py_err(e, None))?;
            let writes = validate_write_results("pwrite_many", &ops, writes)?;
            Ok(writes.into_iter().map(|write| write.written).collect())
        })
    }

    /// Append to open O_APPEND descriptors in one vector operation, returning
    /// each byte count and resulting position.
    fn append_many(
        &self,
        py: Python<'_>,
        fds: Vec<i64>,
        datas: Vec<Vec<u8>>,
    ) -> PyResult<Vec<(usize, u64)>> {
        if fds.len() != datas.len() {
            return Err(PyValueError::new_err(
                "fds and datas must have equal lengths",
            ));
        }
        let ops: Vec<WriteOp> = fds
            .into_iter()
            .zip(datas)
            .map(|(fd, data)| WriteOp::new(VfFile::from_fd(fd as i32), VfOffset::Cur, data))
            .collect();
        self.with_fs(py, move |fs| {
            let writes = fs.writev(&ops).map_err(|e| to_py_err(e, None))?;
            let writes = validate_write_results("append_many", &ops, writes)?;
            writes
                .into_iter()
                .map(|write| {
                    let position = write
                        .offset
                        .checked_add(write.written as u64)
                        .ok_or_else(|| PyOSError::new_err("file position overflow"))?;
                    Ok((write.written, position))
                })
                .collect()
        })
    }

    fn truncate(&self, py: Python<'_>, path: PathBuf, size: u64) -> PyResult<()> {
        let a = VfAttrs {
            file: VfFile::from_os_path(&path),
            masks: AttrMask::SIZE,
            size,
            ..VfAttrs::default()
        };
        self.with_fs(py, move |fs| {
            fs.setattrsv(std::slice::from_ref(&a))
                .map_err(|e| to_py_err(e, Some(path.as_path())))
        })
    }

    fn touch(&self, py: Python<'_>, path: PathBuf) -> PyResult<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| PyOSError::new_err("system clock is before the Unix epoch"))?;
        let a = VfAttrs {
            file: VfFile::from_os_path(&path),
            masks: AttrMask::ATIME | AttrMask::MTIME,
            atime_sec: now.as_secs().min(i64::MAX as u64) as i64,
            atime_nsec: now.subsec_nanos(),
            mtime_sec: now.as_secs().min(i64::MAX as u64) as i64,
            mtime_nsec: now.subsec_nanos(),
            ..VfAttrs::default()
        };
        self.with_fs(py, move |fs| {
            fs.setattrsv(std::slice::from_ref(&a))
                .map_err(|e| to_py_err(e, Some(path.as_path())))
        })
    }

    fn chmod(&self, py: Python<'_>, path: PathBuf, mode: u32) -> PyResult<()> {
        let a = VfAttrs {
            file: VfFile::from_os_path(&path),
            masks: AttrMask::MODE,
            mode,
            ..VfAttrs::default()
        };
        self.with_fs(py, move |fs| {
            fs.setattrsv(std::slice::from_ref(&a))
                .map_err(|e| to_py_err(e, Some(path.as_path())))
        })
    }

    fn mkdir(&self, py: Python<'_>, path: PathBuf, mode: u32) -> PyResult<()> {
        self.with_fs(py, move |fs| {
            fs.mkdir(&path, mode)
                .map_err(|e| to_py_err(e, Some(path.as_path())))
        })
    }

    /// Create `path` and all missing ancestors.
    fn ensure_dir(&self, py: Python<'_>, path: PathBuf, mode: u32) -> PyResult<()> {
        self.with_fs(py, move |fs| {
            fs.ensure_dir(&path, mode)
                .map_err(|e| to_py_err(e, Some(path.as_path())))
        })
    }

    fn symlink(&self, py: Python<'_>, target: PathBuf, path: PathBuf) -> PyResult<()> {
        self.with_fs(py, move |fs| {
            fs.symlink(&target, &path)
                .map_err(|e| to_py_err(e, Some(path.as_path())))
        })
    }

    fn readlink(&self, py: Python<'_>, path: PathBuf) -> PyResult<String> {
        self.with_fs(py, move |fs| {
            let b = fs
                .readlink(&path)
                .map_err(|e| to_py_err(e, Some(path.as_path())))?;
            Ok(String::from_utf8_lossy(&b).into_owned())
        })
    }

    fn hardlink(&self, py: Python<'_>, src: PathBuf, dst: PathBuf) -> PyResult<()> {
        self.with_fs(py, move |fs| {
            fs.hardlinkv(&[src.as_path()], &[dst.as_path()])
                .map_err(|e| to_py_err(e, Some(dst.as_path())))
        })
    }

    fn getcwd(&self, py: Python<'_>) -> PyResult<Py<PyString>> {
        let cwd = self.with_fs(py, |fs| Ok(fs.getcwd()))?;
        Ok(cwd.as_os_str().into_pyobject(py)?.unbind())
    }

    fn chdir(&self, py: Python<'_>, path: PathBuf) -> PyResult<()> {
        self.with_fs(py, move |fs| {
            fs.chdir(&path)
                .map_err(|e| to_py_err(e, Some(path.as_path())))
        })
    }

    // -- batched -------------------------------------------------------------------

    /// Stat many paths in batches. Returns `(results, errors)` where
    /// `results[i]` is the attribute dict (or None on failure) and `errors`
    /// maps an index to its errno.
    fn stat_many(&self, py: Python<'_>, paths: Vec<PathBuf>) -> PyResult<StatManyResult> {
        let (attrs, errors) = self.with_fs(py, move |fs| {
            attrs_many_impl(fs, &paths, full_mask(), true).map_err(|e| to_py_err(e, None))
        })?;
        let mut out = Vec::with_capacity(attrs.len());
        for a in attrs {
            out.push(match a {
                Some(a) => Some(attrs_to_dict(py, &a)?),
                None => None,
            });
        }
        Ok((out, errors))
    }

    /// lstat many paths in batches (used by `exists`/`exists_many`).
    fn lstat_many(&self, py: Python<'_>, paths: Vec<PathBuf>) -> PyResult<StatManyResult> {
        let (attrs, errors) = self.with_fs(py, move |fs| {
            let follow = fs.capabilities() & VF_CAP_LSTAT == 0;
            attrs_many_impl(fs, &paths, full_mask(), follow).map_err(|e| to_py_err(e, None))
        })?;
        let mut out = Vec::with_capacity(attrs.len());
        for a in attrs {
            out.push(match a {
                Some(a) => Some(attrs_to_dict(py, &a)?),
                None => None,
            });
        }
        Ok((out, errors))
    }

    /// Whether each path exists (lstat semantics: a dangling symlink exists).
    /// Non-NOENT failures raise.
    fn exists_many(&self, py: Python<'_>, paths: Vec<PathBuf>) -> PyResult<Vec<bool>> {
        self.with_fs(py, move |fs| {
            let follow = fs.capabilities() & VF_CAP_LSTAT == 0;
            let (attrs, errors) = attrs_many_impl(fs, &paths, AttrMask::stat(), follow)
                .map_err(|e| to_py_err(e, None))?;
            let mut first_err: Option<VfError> = None;
            let mut out = Vec::with_capacity(paths.len());
            for (i, a) in attrs.iter().enumerate() {
                if let Some(err) = errors.get(&i) {
                    if *err != ERR_NOENT && first_err.is_none() {
                        first_err = Some(VfError::failure(i, *err));
                    }
                    out.push(false);
                } else {
                    out.push(a.is_some());
                }
            }
            match first_err {
                Some(e) => Err(to_py_err(e, None)),
                None => Ok(out),
            }
        })
    }

    /// Read byte ranges in batches. `ends[i]` is the exclusive end (None =
    /// until EOF, requiring a batched size fetch). Returns `(data, errors)`
    /// with per-path bytes (None on failure) and an errno map.
    #[pyo3(signature = (paths, starts, ends=None))]
    fn read_many(
        &self,
        py: Python<'_>,
        paths: Vec<PathBuf>,
        starts: Vec<u64>,
        ends: Option<Vec<Option<u64>>>,
    ) -> PyResult<ReadManyResult> {
        if starts.len() != paths.len() {
            return Err(PyValueError::new_err(
                "starts length must match paths length",
            ));
        }
        let ends = match ends {
            Some(e) => {
                if e.len() != paths.len() {
                    return Err(PyValueError::new_err("ends length must match paths length"));
                }
                e
            }
            None => vec![None; paths.len()],
        };

        self.with_fs(py, move |fs| {
            let mut results: Vec<Option<Vec<u8>>> = vec![None; paths.len()];
            let mut errors: HashMap<usize, u32> = HashMap::new();

            // Resolve lengths; batch-fetch sizes when any end is None.
            let mut lengths: Vec<usize> = vec![0; paths.len()];
            let mut stat_errors: HashMap<usize, u32> = HashMap::new();
            let need_sizes = ends.iter().any(Option::is_none);
            if need_sizes {
                let (attrs, errs) = attrs_many_impl(fs, &paths, AttrMask::SIZE, true)
                    .map_err(|e| to_py_err(e, None))?;
                errors.extend(errs.iter().map(|(&k, &v)| (k, v)));
                stat_errors = errs;
                for (i, a) in attrs.iter().enumerate() {
                    if let Some(a) = a {
                        let end = ends[i].unwrap_or(a.size);
                        lengths[i] = end.saturating_sub(starts[i]).min(usize::MAX as u64) as usize;
                    }
                }
            } else {
                for (i, end) in ends.iter().enumerate() {
                    lengths[i] = end
                        .ok_or_else(|| {
                            contract_error("read_many", "missing range end after validation")
                        })?
                        .saturating_sub(starts[i])
                        .min(usize::MAX as u64) as usize;
                }
            }

            // Batch reads with per-index error retry.
            let mut remaining: Vec<usize> = (0..paths.len())
                .filter(|&i| !stat_errors.contains_key(&i))
                .collect();
            while !remaining.is_empty() {
                let ops: Vec<ReadOp> = remaining
                    .iter()
                    .map(|&i| ReadOp::at(VfFile::from_os_path(&paths[i]), starts[i], lengths[i]))
                    .collect();
                match fs.readv(&ops) {
                    Ok(res) => {
                        let res = validate_read_results("read_many", &ops, res)?;
                        for (&i, r) in remaining.iter().zip(res) {
                            results[i] = Some(r.data);
                        }
                        break;
                    }
                    Err(e) => {
                        let Some(bi) = e.index_opt() else {
                            return Err(to_py_err(e, None));
                        };
                        if bi >= remaining.len() {
                            return Err(to_py_err(e, None));
                        }
                        let orig = remaining[bi];
                        errors.insert(orig, e.err_no());
                        remaining.remove(bi);
                    }
                }
            }
            Ok((results, errors))
        })
    }

    /// Read every file in full (offset 0 to EOF) in batched, no-stat reads.
    /// Returns per-path bytes (None on failure) and an errno map.
    fn read_all_many(&self, py: Python<'_>, paths: Vec<PathBuf>) -> PyResult<ReadManyResult> {
        let max_total_bytes = self.read_all_max_total_bytes;
        self.with_fs(py, move |fs| {
            read_allv_impl(fs, &paths, max_total_bytes).map_err(|e| to_py_err(e, None))
        })
    }

    /// Write files at offset 0 (creating them), one writev batch; with
    /// `truncate=True` each file is truncated to zero in the same compound.
    /// Returns the number of bytes written per file.
    #[pyo3(signature = (paths, datas, truncate=true))]
    fn write_many(
        &self,
        py: Python<'_>,
        paths: Vec<PathBuf>,
        datas: Vec<Vec<u8>>,
        truncate: bool,
    ) -> PyResult<Vec<usize>> {
        if paths.len() != datas.len() {
            return Err(PyValueError::new_err("paths and datas length must match"));
        }
        let ops: Vec<WriteOp> = paths
            .iter()
            .zip(datas)
            .map(|(p, d)| {
                let op = WriteOp::at(VfFile::from_os_path(p), 0, d).with_creation();
                if truncate { op.with_truncate() } else { op }
            })
            .collect();
        self.with_fs(py, move |fs| {
            let res = fs.writev(&ops).map_err(|e| map_err_with_path(e, &paths))?;
            let res = validate_write_results("write_many", &ops, res)?;
            Ok(res.into_iter().map(|r| r.written).collect())
        })
    }

    /// Truncate many files to `sizes` in one setattrsv batch.
    fn truncate_many(&self, py: Python<'_>, paths: Vec<PathBuf>, sizes: Vec<u64>) -> PyResult<()> {
        if paths.len() != sizes.len() {
            return Err(PyValueError::new_err("paths and sizes length must match"));
        }
        let attrs: Vec<VfAttrs> = paths
            .iter()
            .zip(sizes)
            .map(|(p, s)| VfAttrs {
                file: VfFile::from_os_path(p),
                masks: AttrMask::SIZE,
                size: s,
                ..VfAttrs::default()
            })
            .collect();
        self.with_fs(py, move |fs| {
            fs.setattrsv(&attrs)
                .map_err(|e| map_err_with_path(e, &paths))
        })
    }

    /// Create directories in one mkdirv batch.
    fn mkdir_many(&self, py: Python<'_>, paths: Vec<PathBuf>, mode: u32) -> PyResult<()> {
        let attrs: Vec<VfAttrs> = paths
            .iter()
            .map(|p| VfAttrs {
                file: VfFile::from_os_path(p),
                masks: AttrMask::MODE,
                mode,
                ..VfAttrs::default()
            })
            .collect();
        self.with_fs(py, move |fs| {
            fs.mkdirv(&attrs).map_err(|e| map_err_with_path(e, &paths))
        })
    }

    /// Open many files in one openv batch; returns descriptors.
    fn open_many(
        &self,
        py: Python<'_>,
        paths: Vec<PathBuf>,
        modes: Vec<String>,
    ) -> PyResult<Vec<i64>> {
        if paths.len() != modes.len() {
            return Err(PyValueError::new_err("paths and modes length must match"));
        }
        let flags: Vec<i32> = modes
            .iter()
            .map(|m| mode_to_flags(m))
            .collect::<PyResult<_>>()?;
        let modes: Vec<u32> = vec![0o644; paths.len()];
        self.with_fs(py, move |fs| {
            let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
            let files = fs
                .openv(&refs, &flags, &modes)
                .map_err(|e| map_err_with_path(e, &paths))?;
            if files.len() != paths.len() {
                return Err(contract_error(
                    "open_many",
                    format!(
                        "expected {} results, received {}",
                        paths.len(),
                        files.len()
                    ),
                ));
            }
            files
                .into_iter()
                .map(|file| descriptor("open_many", &file))
                .collect::<PyResult<Vec<_>>>()
        })
    }

    /// Close many descriptors in one closev batch.
    fn close_many(&self, py: Python<'_>, fds: Vec<i64>) -> PyResult<()> {
        let files: Vec<VfFile> = fds.iter().map(|&fd| VfFile::from_fd(fd as i32)).collect();
        self.with_fs(py, move |fs| {
            fs.closev(&files).map_err(|e| to_py_err(e, None))
        })
    }

    /// List one directory; returns entry attribute dicts.
    fn listdir(&self, py: Python<'_>, path: PathBuf) -> PyResult<Vec<Py<PyDict>>> {
        let max_entries = self.directory_max_entries;
        let max_path_bytes = self.directory_max_path_bytes;
        let entries = self.with_fs(py, move |fs| {
            let entries = fs
                .listdir(&path, full_mask(), max_entries.saturating_add(1), false)
                .map_err(|e| to_py_err(e, Some(path.as_path())))?;
            validate_directory_results(&entries, max_entries, max_path_bytes)
                .map_err(|e| to_py_err(e, Some(path.as_path())))?;
            Ok(entries)
        })?;
        entries.iter().map(|e| attrs_to_dict(py, e)).collect()
    }

    /// List several directories; one native call for the Python side.
    #[pyo3(signature = (paths, recursive=false))]
    fn listdir_many(
        &self,
        py: Python<'_>,
        paths: Vec<PathBuf>,
        recursive: bool,
    ) -> PyResult<Vec<Vec<Py<PyDict>>>> {
        let max_entries = self.directory_max_entries;
        let max_path_bytes = self.directory_max_path_bytes;
        let max_depth = self.walk_max_depth;
        let groups = self.with_fs(py, move |fs| {
            let mut groups = Vec::with_capacity(paths.len());
            for p in &paths {
                let entries = if recursive {
                    let mut no_sort = |_dir: &Path, _attrs: &mut Vec<VfAttrs>| {};
                    fs.walk_with_options(
                        p,
                        full_mask(),
                        WalkOptions::new()
                            .max_entries(max_entries)
                            .max_path_bytes(max_path_bytes)
                            .max_depth(max_depth),
                        &mut no_sort,
                    )
                    .map_err(|e| to_py_err(e, Some(p.as_path())))?
                    .into_iter()
                    .flat_map(|entry| entry.entries)
                    .collect()
                } else {
                    let entries = fs
                        .listdir(p, full_mask(), max_entries.saturating_add(1), false)
                        .map_err(|e| to_py_err(e, Some(p.as_path())))?;
                    validate_directory_results(&entries, max_entries, max_path_bytes)
                        .map_err(|e| to_py_err(e, Some(p.as_path())))?;
                    entries
                };
                groups.push(entries);
            }
            Ok(groups)
        })?;
        let mut out = Vec::with_capacity(groups.len());
        for entries in groups {
            out.push(
                entries
                    .iter()
                    .map(|e| attrs_to_dict(py, e))
                    .collect::<PyResult<Vec<_>>>()?,
            );
        }
        Ok(out)
    }

    /// Walk a tree in one call (the NFS backend lists each level in batched
    /// compounds). Returns `(dir_path, entries)` per directory in pre-order.
    #[pyo3(signature = (root, sort=true, max_depth=None))]
    fn walk(
        &self,
        py: Python<'_>,
        root: PathBuf,
        sort: bool,
        max_depth: Option<usize>,
    ) -> PyResult<WalkResult> {
        let options = WalkOptions::new()
            .max_entries(self.directory_max_entries)
            .max_path_bytes(self.directory_max_path_bytes)
            .max_depth(
                max_depth
                    .unwrap_or(self.walk_max_depth)
                    .min(self.walk_max_depth),
            )
            .truncate_at_max_depth(max_depth.is_some_and(|depth| depth <= self.walk_max_depth));
        let tree = self.with_fs(py, move |fs| {
            let mut sort_fn = |_dir: &Path, attrs: &mut Vec<VfAttrs>| {
                if sort {
                    attrs.sort_by(|a, b| {
                        a.file
                            .path()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default()
                            .cmp(
                                &b.file
                                    .path()
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                            )
                    });
                }
            };
            fs.walk_with_options(&root, full_mask(), options, &mut sort_fn)
                .map_err(|e| to_py_err(e, Some(root.as_path())))
        })?;
        let mut out = Vec::with_capacity(tree.len());
        for w in tree {
            let mut entries = Vec::with_capacity(w.entries.len());
            for e in &w.entries {
                entries.push(attrs_to_dict(py, e)?);
            }
            let dir = w.path.as_os_str().into_pyobject(py)?;
            out.push((dir.unbind(), entries));
        }
        Ok(out)
    }

    /// Remove paths in batches (one removev compound per parent directory).
    fn remove_many(&self, py: Python<'_>, paths: Vec<PathBuf>) -> PyResult<()> {
        let files: Vec<VfFile> = paths
            .iter()
            .map(|path| VfFile::from_os_path(path.as_path()))
            .collect();
        self.with_fs(py, move |fs| {
            fs.removev(&files).map_err(|e| map_err_with_path(e, &paths))
        })
    }

    /// Rename pairs in one renamev batch.
    fn rename_many(&self, py: Python<'_>, pairs: Vec<(PathBuf, PathBuf)>) -> PyResult<()> {
        let files: Vec<(VfFile, VfFile)> = pairs
            .iter()
            .map(|(a, b)| (VfFile::from_os_path(a), VfFile::from_os_path(b)))
            .collect();
        self.with_fs(py, move |fs| {
            fs.renamev(&files).map_err(|e| {
                let path = e
                    .index_opt()
                    .and_then(|index| pairs.get(index))
                    .map(|(source, _)| source.as_path());
                to_py_err(e, path)
            })
        })
    }

    /// Copy whole files in batches (no-stat read_allv + truncating writev,
    /// each constant in the number of compounds for one-dir batches).
    /// Returns `(copied_bytes, errors)`.
    fn copy_many(
        &self,
        py: Python<'_>,
        pairs: Vec<(PathBuf, PathBuf)>,
    ) -> PyResult<CopyManyResult> {
        let sources: Vec<PathBuf> = pairs.iter().map(|(s, _)| s.clone()).collect();
        let dests: Vec<PathBuf> = pairs.iter().map(|(_, d)| d.clone()).collect();
        let n = pairs.len();
        let max_total_bytes = self.read_all_max_total_bytes;
        self.with_fs(py, move |fs| {
            let mut copied: Vec<Option<u64>> = vec![None; n];
            let mut errors: HashMap<usize, u32> = HashMap::new();

            // 1+2. Read whole files (no separate size-stat round trip).
            let (mut data, read_errors) =
                read_allv_impl(fs, &sources, max_total_bytes).map_err(|e| to_py_err(e, None))?;
            errors.extend(read_errors.iter().map(|(&k, &v)| (k, v)));

            // 3. writes. The in-compound O_TRUNC truncates each destination to
            // zero; writing the whole source at offset zero then leaves exactly
            // the source size (no separate truncate compound is needed).
            let mut remaining: Vec<usize> = (0..n).filter(|&i| data[i].is_some()).collect();
            while !remaining.is_empty() {
                let ops: Vec<WriteOp> = remaining
                    .iter()
                    .map(|&i| {
                        WriteOp::at(
                            VfFile::from_os_path(&dests[i]),
                            0,
                            data[i].clone().unwrap_or_default(),
                        )
                        .with_creation()
                        .with_truncate()
                    })
                    .collect();
                match fs.writev(&ops) {
                    Ok(res) => {
                        let res = validate_write_results("copy_many", &ops, res)?;
                        for (&i, r) in remaining.iter().zip(res) {
                            copied[i] = Some(r.written as u64);
                        }
                        break;
                    }
                    Err(e) => {
                        let Some(bi) = e.index_opt() else {
                            return Err(to_py_err(e, None));
                        };
                        if bi >= remaining.len() {
                            return Err(to_py_err(e, None));
                        }
                        let orig = remaining[bi];
                        errors.insert(orig, e.err_no());
                        data[orig] = None;
                        remaining.remove(bi);
                    }
                }
            }
            Ok((copied, errors))
        })
    }

    /// Recursively copy a directory tree (the backend batches per level).
    #[pyo3(signature = (src, dst, symlinks=false))]
    fn cp_recursive(
        &self,
        py: Python<'_>,
        src: PathBuf,
        dst: PathBuf,
        symlinks: bool,
    ) -> PyResult<()> {
        self.with_fs(py, move |fs| {
            fs.cp_recursive(&src, &dst, symlinks, false)
                .map_err(|e| to_py_err(e, Some(src.as_path())))
        })
    }

    /// Remove paths, recursively when requested.
    fn rm(&self, py: Python<'_>, paths: Vec<PathBuf>, recursive: bool) -> PyResult<()> {
        self.with_fs(py, move |fs| {
            let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
            fs.rm(&refs, recursive)
                .map_err(|e| map_err_with_path(e, &paths))
        })
    }

    // -- diagnostics ---------------------------------------------------------------

    /// Aggregate compound statistics since the last read (reset-on-read).
    fn compound_stats(&self) -> (u64, u64, u64, u64) {
        compound_stats()
    }

    fn rpc_stats(&self) -> (u64, u64) {
        rpc_stats()
    }
}

/// Aggregate compound statistics (count, ops, bytes, max ops), resetting.
#[pyfunction]
fn compound_stats_py() -> (u64, u64, u64, u64) {
    compound_stats()
}

/// Aggregate RPC round-trip statistics (calls, microseconds), resetting.
#[pyfunction]
fn rpc_stats_py() -> (u64, u64) {
    rpc_stats()
}

/// Register the shared native API in one protocol package's extension module.
pub fn register(m: &Bound<'_, PyModule>, version: &str) -> PyResult<()> {
    m.add_class::<NfsClient>()?;
    m.add_function(wrap_pyfunction!(compound_stats_py, m)?)?;
    m.add_function(wrap_pyfunction!(rpc_stats_py, m)?)?;
    m.add("__version__", version)?;
    m.add("CAP_SERVER_COPY", VF_CAP_SERVER_COPY)?;
    m.add("CAP_POSIX_METADATA", VF_CAP_POSIX_METADATA)?;
    m.add("CAP_SYMLINKS", VF_CAP_SYMLINKS)?;
    m.add("CAP_HARDLINKS", VF_CAP_HARDLINKS)?;
    m.add("CAP_NON_UTF8_PATHS", VF_CAP_NON_UTF8_PATHS)?;
    m.add("CAP_LSTAT", VF_CAP_LSTAT)?;
    m.add("ERR_UNSUPPORTED", VF_ERR_UNSUPPORTED)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_read_result_contract_is_validated() {
        let op = ReadOp::at(VfFile::from_fd(7), 11, 3);
        assert!(one_read_result("pread", &op, Vec::new()).is_err());
        assert!(
            one_read_result(
                "pread",
                &op,
                vec![ReadResult {
                    file: VfFile::from_fd(8),
                    offset: 11,
                    data: vec![1],
                    eof: false,
                }],
            )
            .is_err()
        );

        let second = ReadOp::at(VfFile::from_fd(8), 0, 1);
        let swapped = vec![
            ReadResult {
                file: second.file.clone(),
                offset: 0,
                data: vec![2],
                eof: false,
            },
            ReadResult {
                file: op.file.clone(),
                offset: 11,
                data: vec![1],
                eof: false,
            },
        ];
        assert!(validate_read_results("read_many", &[op.clone(), second], swapped).is_err());
        assert!(
            one_read_result(
                "pread",
                &op,
                vec![ReadResult {
                    file: VfFile::from_fd(7),
                    offset: 12,
                    data: vec![1],
                    eof: false,
                }],
            )
            .is_err()
        );
        assert!(
            one_read_result(
                "pread",
                &op,
                vec![ReadResult {
                    file: VfFile::from_fd(7),
                    offset: 11,
                    data: vec![1; 4],
                    eof: false,
                }],
            )
            .is_err()
        );
    }

    #[test]
    fn scalar_write_result_contract_is_validated() {
        let op = WriteOp::at(VfFile::from_fd(7), 11, vec![1, 2, 3]);
        assert!(one_write_result("pwrite", &op, Vec::new()).is_err());
        assert!(
            one_write_result(
                "pwrite",
                &op,
                vec![WriteResult {
                    file: VfFile::from_fd(7),
                    offset: 11,
                    written: 4,
                    stable: true,
                }],
            )
            .is_err()
        );
    }

    #[test]
    fn path_result_cannot_cross_the_descriptor_boundary() {
        assert!(descriptor("open", &VfFile::from_path("/not-a-descriptor")).is_err());
    }
}
