//! Python bindings for the vectorized vnfs NFSv4.1 client.
//!
//! This module exposes the [`VecFs`] surface as a `NfsClient` PyO3 class
//! under `nfs4fs._native`. Every fsspec bulk operation funnels through the
//! vectorized calls here (getattrsv/lgetattrsv, readv, writev, openv/closev,
//! removev, renamev, dupv, walk), so round trips scale with the number of
//! batches/directories rather than the number of files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pyo3::exceptions::{
    PyConnectionError, PyFileExistsError, PyFileNotFoundError, PyIsADirectoryError,
    PyNotADirectoryError, PyNotImplementedError, PyOSError, PyPermissionError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyString};

use vnfs::compound::{compound_stats, rpc_stats};
use vnfs::dummy_vecfs::DummyVecFs;
use vnfs::nfs::NfsVecFs;
use vnfs::smb::SmbVecFs;
use vnfs::vecfs::{
    AttrMask, ERR_ACCES, ERR_EXIST, ERR_INVAL, ERR_ISDIR, ERR_NOENT, ERR_NOTDIR, ReadOp, SeekFrom,
    VF_ERR_UNSUPPORTED, VfAttrs, VfError, VfFile, VfOffset, VfType, WriteOp,
};

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

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn lock_err<T>(_: std::sync::PoisonError<T>) -> PyErr {
    PyErr::new::<PyOSError, _>("vnfs client lock poisoned")
}

/// Map a `VfError` onto the Python exception class that matches its errno.
fn to_py_err(e: VfError, path: Option<&Path>) -> PyErr {
    let what = path
        .map(|p| format!(": '{}'", p.display()))
        .unwrap_or_default();
    match e {
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
    }
}

/// Attach the failing operation's path to an error from a batched call.
fn map_err_with_path(e: VfError, paths: &[PathBuf]) -> PyErr {
    let idx = e.index();
    to_py_err(e, paths.get(idx).map(PathBuf::as_path))
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
        // fileid doubles as the version checksum.
        d.set_item("checksum", a.fileid)?;
    }
    if r.contains(AttrMask::BLOCKS) {
        d.set_item("blocks", a.blocks)?;
    }
    if r.contains(AttrMask::CTIME) {
        d.set_item("created", a.ctime_sec)?;
    }
    if r.contains(AttrMask::MTIME) {
        d.set_item("modified", a.mtime_sec)?;
    }
    if r.contains(AttrMask::ATIME) {
        d.set_item("accessed", a.atime_sec)?;
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
    fs: &mut dyn vnfs::VecFs,
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
                let bi = bi.min(remaining.len() - 1);
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
fn read_allv_impl(fs: &mut dyn vnfs::VecFs, paths: &[PathBuf]) -> Result<ReadManyResult, VfError> {
    let files: Vec<VfFile> = paths.iter().map(|p| VfFile::from_os_path(p)).collect();
    let mut results: Vec<Option<Vec<u8>>> = vec![None; paths.len()];
    let mut errors: ErrnoMap = HashMap::new();
    let mut remaining: Vec<usize> = (0..paths.len()).collect();
    while !remaining.is_empty() {
        let subset: Vec<VfFile> = remaining.iter().map(|&i| files[i].clone()).collect();
        match fs.read_allv(&subset) {
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
                let bi = bi.min(remaining.len() - 1);
                let orig = remaining[bi];
                errors.insert(orig, e.err_no());
                remaining.remove(bi);
            }
        }
    }
    Ok((results, errors))
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

/// One client (one TCP connection / NFS session) behind a mutex.
#[pyclass(module = "nfs4fs._native")]
struct NfsClient {
    fs: Mutex<Box<dyn vnfs::VecFs + Send>>,
}

#[pymethods]
impl NfsClient {
    /// Connect to an NFS server (`backend="nfs"`, default), an SMB2/3 share
    /// (`backend="smb"`), or a local directory (`backend="dummy"`).
    #[new]
    #[pyo3(signature = (host, backend="nfs", root=None, compound_size_limit=None, minor_version=None, share=None, username="", password="", domain=""))]
    fn new(
        host: &str,
        backend: &str,
        root: Option<PathBuf>,
        compound_size_limit: Option<usize>,
        minor_version: Option<u32>,
        share: Option<&str>,
        username: &str,
        password: &str,
        domain: &str,
    ) -> PyResult<Self> {
        let fs: Box<dyn vnfs::VecFs + Send> = match backend {
            "nfs" => {
                if minor_version.is_some_and(|version| !matches!(version, 1 | 2)) {
                    return Err(PyValueError::new_err(
                        "minor_version must be 1, 2, or None",
                    ));
                }
                let mut nfs = match minor_version {
                    Some(version) => NfsVecFs::connect_minor(host, version),
                    None => NfsVecFs::connect(host),
                }
                .map_err(|e| to_py_err(e, Some(Path::new(host))))?;
                if let Some(limit) = compound_size_limit {
                    nfs.set_max_compound_bytes(limit);
                }
                Box::new(nfs)
            }
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
                )
            }
            "dummy" => {
                let root_path = match root {
                    Some(r) => PathBuf::from(r),
                    None => std::env::temp_dir().join(format!(
                        "nfs4fs_dummy_{}_{}",
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
                Box::new(DummyVecFs::new(root_path))
            }
            other => {
                return Err(PyValueError::new_err(format!(
                    "unknown backend: {:?}",
                    other
                )));
            }
        };
        Ok(NfsClient { fs: Mutex::new(fs) })
    }

    /// Negotiated NFS minor version, or None for the dummy backend.
    fn minor_version(&self) -> PyResult<Option<u32>> {
        let fs = self.fs.lock().map_err(lock_err)?;
        Ok(fs.nfs_minorversion())
    }

    /// Negotiated SMB dialect revision, or None for non-SMB backends.
    fn smb_dialect(&self) -> PyResult<Option<u16>> {
        let fs = self.fs.lock().map_err(lock_err)?;
        Ok(fs.smb_dialect())
    }

    /// Current backend capability bitset (see CAP_SERVER_COPY).
    fn capabilities(&self) -> PyResult<u64> {
        let fs = self.fs.lock().map_err(lock_err)?;
        Ok(fs.capabilities())
    }

    /// Whether NFSv4.2 server COPY is currently enabled.
    fn server_copy_enabled(&self) -> PyResult<bool> {
        let fs = self.fs.lock().map_err(lock_err)?;
        Ok(fs.capabilities() & vnfs::vecfs::VF_CAP_SERVER_COPY != 0)
    }

    // -- single-op ------------------------------------------------------------------

    /// Stat `path` (follows symlinks), returning an attribute dict.
    fn stat(&self, py: Python<'_>, path: PathBuf) -> PyResult<Py<PyDict>> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let mut a = VfAttrs {
            file: VfFile::from_os_path(&path),
            masks: full_mask(),
            ..VfAttrs::default()
        };
        fs.getattrsv(std::slice::from_mut(&mut a))
            .map_err(|e| to_py_err(e, Some(path.as_path())))?;
        attrs_to_dict(py, &a)
    }

    /// lstat `path` (does not follow symlinks).
    fn lstat(&self, py: Python<'_>, path: PathBuf) -> PyResult<Py<PyDict>> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let mut a = VfAttrs {
            file: VfFile::from_os_path(&path),
            masks: full_mask(),
            ..VfAttrs::default()
        };
        fs.lgetattrsv(std::slice::from_mut(&mut a))
            .map_err(|e| to_py_err(e, Some(path.as_path())))?;
        attrs_to_dict(py, &a)
    }

    fn exists(&self, path: PathBuf) -> PyResult<bool> {
        Ok(self.exists_many(vec![path.clone()])?[0])
    }

    /// Open a file; returns the backend descriptor (an int).
    fn open(&self, path: PathBuf, mode: &str) -> PyResult<i64> {
        let flags = mode_to_flags(mode)?;
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let f = fs
            .open(&path, flags, 0o644)
            .map_err(|e| to_py_err(e, Some(path.as_path())))?;
        Ok(f.fd().expect("open returns a descriptor") as i64)
    }

    fn close(&self, fd: i64) -> PyResult<()> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.close(&VfFile::from_fd(fd as i32))
            .map_err(|e| to_py_err(e, None))
    }

    /// Read `length` bytes at the descriptor's current position (advances it).
    fn read(&self, fd: i64, length: usize) -> PyResult<Vec<u8>> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let r = fs
            .readv(&[ReadOp::new(
                VfFile::from_fd(fd as i32),
                VfOffset::Cur,
                length,
            )])
            .map_err(|e| to_py_err(e, None))?;
        Ok(r.into_iter().next().expect("one result").data)
    }

    /// Write `data` at the descriptor's current position (advances it).
    fn write(&self, fd: i64, data: Vec<u8>) -> PyResult<usize> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let w = fs
            .writev(&[WriteOp::new(
                VfFile::from_fd(fd as i32),
                VfOffset::Cur,
                data,
            )])
            .map_err(|e| to_py_err(e, None))?;
        Ok(w.into_iter().next().expect("one result").written)
    }

    /// Read `length` bytes at an absolute offset (does not move the position).
    fn pread(&self, fd: i64, length: usize, offset: u64) -> PyResult<Vec<u8>> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let r = fs
            .readv(&[ReadOp::new(
                VfFile::from_fd(fd as i32),
                VfOffset::At(offset),
                length,
            )])
            .map_err(|e| to_py_err(e, None))?;
        Ok(r.into_iter().next().expect("one result").data)
    }

    /// Write `data` at an absolute offset.
    fn pwrite(&self, fd: i64, data: Vec<u8>, offset: u64) -> PyResult<usize> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let w = fs
            .writev(&[WriteOp::new(
                VfFile::from_fd(fd as i32),
                VfOffset::At(offset),
                data,
            )])
            .map_err(|e| to_py_err(e, None))?;
        Ok(w.into_iter().next().expect("one result").written)
    }

    /// `fseek`; `whence` is 0=SET, 1=CUR, 2=END. Returns the new position.
    fn fseek(&self, fd: i64, offset: i64, whence: i32) -> PyResult<i64> {
        let whence = match whence {
            0 => SeekFrom::Set,
            1 => SeekFrom::Cur,
            2 => SeekFrom::End,
            _ => return Err(PyValueError::new_err("whence must be 0, 1 or 2")),
        };
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.fseek(&VfFile::from_fd(fd as i32), offset, whence)
            .map_err(|e| to_py_err(e, None))
    }

    fn fstat(&self, py: Python<'_>, fd: i64) -> PyResult<Py<PyDict>> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let mut a = VfAttrs {
            file: VfFile::from_fd(fd as i32),
            masks: full_mask(),
            ..VfAttrs::default()
        };
        fs.getattrsv(std::slice::from_mut(&mut a))
            .map_err(|e| to_py_err(e, None))?;
        attrs_to_dict(py, &a)
    }

    fn truncate(&self, path: PathBuf, size: u64) -> PyResult<()> {
        let a = VfAttrs {
            file: VfFile::from_os_path(&path),
            masks: AttrMask::SIZE,
            size,
            ..VfAttrs::default()
        };
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.setattrsv(std::slice::from_ref(&a))
            .map_err(|e| to_py_err(e, Some(path.as_path())))
    }

    fn chmod(&self, path: PathBuf, mode: u32) -> PyResult<()> {
        let a = VfAttrs {
            file: VfFile::from_os_path(&path),
            masks: AttrMask::MODE,
            mode,
            ..VfAttrs::default()
        };
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.setattrsv(std::slice::from_ref(&a))
            .map_err(|e| to_py_err(e, Some(path.as_path())))
    }

    fn mkdir(&self, path: PathBuf, mode: u32) -> PyResult<()> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.mkdir(&path, mode).map_err(|e| to_py_err(e, Some(path.as_path())))
    }

    /// Create `path` and all missing ancestors.
    fn ensure_dir(&self, path: PathBuf, mode: u32) -> PyResult<()> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.ensure_dir(&path, mode)
            .map_err(|e| to_py_err(e, Some(path.as_path())))
    }

    fn symlink(&self, target: PathBuf, path: PathBuf) -> PyResult<()> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.symlink(&target, &path)
            .map_err(|e| to_py_err(e, Some(path.as_path())))
    }

    fn readlink(&self, path: PathBuf) -> PyResult<String> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let b = fs.readlink(&path).map_err(|e| to_py_err(e, Some(path.as_path())))?;
        Ok(String::from_utf8_lossy(&b).into_owned())
    }

    fn hardlink(&self, src: PathBuf, dst: PathBuf) -> PyResult<()> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.hardlinkv(&[src.as_path()], &[dst.as_path()])
            .map_err(|e| to_py_err(e, Some(dst.as_path())))
    }

    fn getcwd(&self, py: Python<'_>) -> PyResult<Py<PyString>> {
        let fs = self.fs.lock().map_err(lock_err)?;
        Ok(fs.getcwd().as_os_str().into_pyobject(py)?.unbind())
    }

    fn chdir(&self, path: PathBuf) -> PyResult<()> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.chdir(&path).map_err(|e| to_py_err(e, Some(path.as_path())))
    }

    // -- batched -------------------------------------------------------------------

    /// Stat many paths in batches. Returns `(results, errors)` where
    /// `results[i]` is the attribute dict (or None on failure) and `errors`
    /// maps an index to its errno.
    fn stat_many(&self, py: Python<'_>, paths: Vec<PathBuf>) -> PyResult<StatManyResult> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let (attrs, errors) = attrs_many_impl(&mut **fs, &paths, full_mask(), true)
            .map_err(|e| to_py_err(e, None))?;
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
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let follow = fs.capabilities() & vnfs::VF_CAP_LSTAT == 0;
        let (attrs, errors) = attrs_many_impl(&mut **fs, &paths, full_mask(), follow)
            .map_err(|e| to_py_err(e, None))?;
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
    fn exists_many(&self, paths: Vec<PathBuf>) -> PyResult<Vec<bool>> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let follow = fs.capabilities() & vnfs::VF_CAP_LSTAT == 0;
        let (attrs, errors) = attrs_many_impl(&mut **fs, &paths, AttrMask::stat(), follow)
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
    }

    /// Read byte ranges in batches. `ends[i]` is the exclusive end (None =
    /// until EOF, requiring a batched size fetch). Returns `(data, errors)`
    /// with per-path bytes (None on failure) and an errno map.
    #[pyo3(signature = (paths, starts, ends=None))]
    fn read_many(
        &self,
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

        let mut fs = self.fs.lock().map_err(lock_err)?;
        let mut results: Vec<Option<Vec<u8>>> = vec![None; paths.len()];
        let mut errors: HashMap<usize, u32> = HashMap::new();

        // Resolve lengths; batch-fetch sizes when any end is None.
        let mut lengths: Vec<usize> = vec![0; paths.len()];
        let mut stat_errors: HashMap<usize, u32> = HashMap::new();
        let need_sizes = ends.iter().any(Option::is_none);
        if need_sizes {
            let (attrs, errs) = attrs_many_impl(&mut **fs, &paths, AttrMask::SIZE, true)
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
                    .expect("all ends present")
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
                    for (&i, r) in remaining.iter().zip(res) {
                        results[i] = Some(r.data);
                    }
                    break;
                }
                Err(e) => {
                    let Some(bi) = e.index_opt() else {
                        return Err(to_py_err(e, None));
                    };
                    let bi = bi.min(remaining.len() - 1);
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
    fn read_all_many(&self, paths: Vec<PathBuf>) -> PyResult<ReadManyResult> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        read_allv_impl(&mut **fs, &paths).map_err(|e| to_py_err(e, None))
    }

    /// Write files at offset 0 (creating them), one writev batch; with
    /// `truncate=True` each file is truncated to zero in the same compound.
    /// Returns the number of bytes written per file.
    #[pyo3(signature = (paths, datas, truncate=true))]
    fn write_many(
        &self,
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
                let op = WriteOp::at(VfFile::from_os_path(&p), 0, d).with_creation();
                if truncate { op.with_truncate() } else { op }
            })
            .collect();
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let res = fs.writev(&ops).map_err(|e| map_err_with_path(e, &paths))?;
        Ok(res.into_iter().map(|r| r.written).collect())
    }

    /// Truncate many files to `sizes` in one setattrsv batch.
    fn truncate_many(&self, paths: Vec<PathBuf>, sizes: Vec<u64>) -> PyResult<()> {
        if paths.len() != sizes.len() {
            return Err(PyValueError::new_err("paths and sizes length must match"));
        }
        let attrs: Vec<VfAttrs> = paths
            .iter()
            .zip(sizes)
            .map(|(p, s)| VfAttrs {
                file: VfFile::from_os_path(&p),
                masks: AttrMask::SIZE,
                size: s,
                ..VfAttrs::default()
            })
            .collect();
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.setattrsv(&attrs)
            .map_err(|e| map_err_with_path(e, &paths))
    }

    /// Create directories in one mkdirv batch.
    fn mkdir_many(&self, paths: Vec<PathBuf>, mode: u32) -> PyResult<()> {
        let attrs: Vec<VfAttrs> = paths
            .iter()
            .map(|p| VfAttrs {
                file: VfFile::from_os_path(&p),
                masks: AttrMask::MODE,
                mode,
                ..VfAttrs::default()
            })
            .collect();
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.mkdirv(&attrs).map_err(|e| map_err_with_path(e, &paths))
    }

    /// Open many files in one openv batch; returns descriptors.
    fn open_many(&self, paths: Vec<PathBuf>, modes: Vec<String>) -> PyResult<Vec<i64>> {
        if paths.len() != modes.len() {
            return Err(PyValueError::new_err("paths and modes length must match"));
        }
        let flags: Vec<i32> = modes
            .iter()
            .map(|m| mode_to_flags(m))
            .collect::<PyResult<_>>()?;
        let modes: Vec<u32> = vec![0o644; paths.len()];
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let files = fs
            .openv(&refs, &flags, &modes)
            .map_err(|e| map_err_with_path(e, &paths))?;
        Ok(files
            .into_iter()
            .map(|f| f.fd().expect("openv returns descriptors") as i64)
            .collect())
    }

    /// Close many descriptors in one closev batch.
    fn close_many(&self, fds: Vec<i64>) -> PyResult<()> {
        let files: Vec<VfFile> = fds.iter().map(|&fd| VfFile::from_fd(fd as i32)).collect();
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.closev(&files).map_err(|e| to_py_err(e, None))
    }

    /// List one directory; returns entry attribute dicts.
    fn listdir(&self, py: Python<'_>, path: PathBuf) -> PyResult<Vec<Py<PyDict>>> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let entries = fs
            .listdir(&path, full_mask(), 0, false)
            .map_err(|e| to_py_err(e, Some(path.as_path())))?;
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
        let mut fs = self.fs.lock().map_err(lock_err)?;
        let mut out = Vec::with_capacity(paths.len());
        for p in &paths {
            let entries = fs
                .listdir(p, full_mask(), 0, recursive)
                .map_err(|e| to_py_err(e, Some(p.as_path())))?;
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
    #[pyo3(signature = (root, sort=true))]
    fn walk(
        &self,
        py: Python<'_>,
        root: PathBuf,
        sort: bool,
    ) -> PyResult<Vec<(Py<PyString>, Vec<Py<PyDict>>)>> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
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
        let tree = fs
            .walk(&root, full_mask(), &mut sort_fn)
            .map_err(|e| to_py_err(e, Some(root.as_path())))?;
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
    fn remove_many(&self, paths: Vec<PathBuf>) -> PyResult<()> {
        let files: Vec<VfFile> = paths.iter().map(|p| VfFile::from_os_path(&p)).collect();
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.removev(&files).map_err(|e| map_err_with_path(e, &paths))
    }

    /// Rename pairs in one renamev batch.
    fn rename_many(&self, pairs: Vec<(PathBuf, PathBuf)>) -> PyResult<()> {
        let files: Vec<(VfFile, VfFile)> = pairs
            .iter()
            .map(|(a, b)| (VfFile::from_os_path(&a), VfFile::from_os_path(&b)))
            .collect();
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.renamev(&files).map_err(|e| {
            let idx = e.index();
            let path = pairs
                .get(idx)
                .map(|(a, _)| a.as_path())
                .or_else(|| pairs.first().map(|(a, _)| a.as_path()));
            to_py_err(e, path)
        })
    }

    /// Copy whole files in batches (no-stat read_allv + truncating writev,
    /// each constant in the number of compounds for one-dir batches).
    /// Returns `(copied_bytes, errors)`.
    fn copy_many(&self, pairs: Vec<(PathBuf, PathBuf)>) -> PyResult<CopyManyResult> {
        let sources: Vec<PathBuf> = pairs.iter().map(|(s, _)| s.clone()).collect();
        let dests: Vec<PathBuf> = pairs.iter().map(|(_, d)| d.clone()).collect();
        let n = pairs.len();
        let mut fs = self.fs.lock().map_err(lock_err)?;

        let mut copied: Vec<Option<u64>> = vec![None; n];
        let mut errors: HashMap<usize, u32> = HashMap::new();

        // 1+2. Read whole files (no separate size-stat round trip).
        let (mut data, read_errors) =
            read_allv_impl(&mut **fs, &sources).map_err(|e| to_py_err(e, None))?;
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
                    for (&i, r) in remaining.iter().zip(res) {
                        copied[i] = Some(r.written as u64);
                    }
                    break;
                }
                Err(e) => {
                    let Some(bi) = e.index_opt() else {
                        return Err(to_py_err(e, None));
                    };
                    let bi = bi.min(remaining.len() - 1);
                    let orig = remaining[bi];
                    errors.insert(orig, e.err_no());
                    data[orig] = None;
                    remaining.remove(bi);
                }
            }
        }
        Ok((copied, errors))
    }

    /// Recursively copy a directory tree (the backend batches per level).
    #[pyo3(signature = (src, dst, symlinks=false))]
    fn cp_recursive(&self, src: PathBuf, dst: PathBuf, symlinks: bool) -> PyResult<()> {
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.cp_recursive(&src, &dst, symlinks, false)
            .map_err(|e| to_py_err(e, Some(src.as_path())))
    }

    /// Remove paths, recursively when requested.
    fn rm(&self, paths: Vec<PathBuf>, recursive: bool) -> PyResult<()> {
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let mut fs = self.fs.lock().map_err(lock_err)?;
        fs.rm(&refs, recursive)
            .map_err(|e| map_err_with_path(e, &paths))
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

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<NfsClient>()?;
    m.add_function(wrap_pyfunction!(compound_stats_py, m)?)?;
    m.add_function(wrap_pyfunction!(rpc_stats_py, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add("CAP_SERVER_COPY", vnfs::vecfs::VF_CAP_SERVER_COPY)?;
    m.add("CAP_POSIX_METADATA", vnfs::VF_CAP_POSIX_METADATA)?;
    m.add("CAP_SYMLINKS", vnfs::VF_CAP_SYMLINKS)?;
    m.add("CAP_HARDLINKS", vnfs::VF_CAP_HARDLINKS)?;
    m.add("CAP_NON_UTF8_PATHS", vnfs::VF_CAP_NON_UTF8_PATHS)?;
    m.add("CAP_LSTAT", vnfs::VF_CAP_LSTAT)?;
    m.add("ERR_UNSUPPORTED", vnfs::VF_ERR_UNSUPPORTED)?;
    Ok(())
}
