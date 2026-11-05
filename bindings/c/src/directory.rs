//! Optional retained child cursors for application-owned directory frontiers.
use super::*;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use vfsi_sync::DirPageCursor;

/// Additive directory-cohort/cursor API; the C ABI remains version three.
pub const VFSI_DIRECTORY_CURSORS_VERSION: u32 = 1;
/// Maximum number of directory operands in one retained-cursor cohort.
pub const VFSI_DIRECTORY_COHORT_MAX: u32 = 32;

/// Opaque, uniquely owned directory seed. Free unused seeds with
/// vfsi_dir_cursor_free. Seeds preserve native parent identity when supported.
pub struct vfsi_dir_cursor {
    owner: Arc<()>,
    path: PathBuf,
    state: Option<DirPageCursor>,
}

pub type vfsi_indexed_listdir_cursor_cb = Option<
    unsafe extern "C" fn(
        usize,
        *const c_char,
        *const c_char,
        *const vfsi_attrs,
        *mut vfsi_dir_cursor,
        *mut c_void,
    ) -> bool,
>;

/// Free a child cursor not submitted for descent. Null is accepted. This
/// performs no directory enumeration and does not need a live filesystem.
///
/// # Safety
/// A non-null cursor must be uniquely owned and originate from this API.
#[no_mangle]
pub unsafe extern "C" fn vfsi_dir_cursor_free(cursor: *mut vfsi_dir_cursor) {
    ffi_guard!((), {
        if !cursor.is_null() {
            // SAFETY: the caller transfers one live, uniquely owned cursor.
            drop(unsafe { Box::from_raw(cursor) });
        }
    })
}

struct Row {
    index: usize,
    name: CString,
    attrs: vfsi_attrs,
    child: Option<Box<vfsi_dir_cursor>>,
}

/// Complete bounded shallow listings for an application-owned frontier.
/// Null cursor slots start fresh directory operands. Non-null slots must be
/// unique child seeds returned by this function for the same filesystem handle
/// and original path. After input validation, ALL slots are consumed and set to null,
/// including on listing failure. Invalid inputs leave the slots unchanged.
///
/// Native child seeds survive between calls. Page continuations stay on the
/// same backend cursor while each cohort is collected, without reopening
/// ancestor paths.
/// Path-based child fallbacks require no-follow resolution. Initial roots use
/// ordinary shallow-listing resolution. All rows/attributes are validated
/// before any callback, with shared entry/path quotas from options.
///
/// Every non-null child passed to the callback transfers ownership to it,
/// including when it returns false. Retain admitted directories for later
/// descent; free excluded/unused children. Undelivered children are freed by
/// this function. Completion/cancellation match vfsi_listdirs. The callback
/// runs outside the backend lock and may call other filesystem operations.
///
/// # Safety
/// The vfsi_listdirs pointer contract applies. cursors and results hold count
/// exclusive elements, disjoint from each other and all other inputs. count
/// is at most VFSI_DIRECTORY_COHORT_MAX. Callback strings/attrs are borrowed
/// only during the call; child cursors are uniquely owned after delivery.
#[no_mangle]
pub unsafe extern "C" fn vfsi_listdirs_from_cursors(
    fs: *mut vfsi_fs,
    dirs: *const *const c_char,
    cursors: *mut *mut vfsi_dir_cursor,
    count: usize,
    options: *const vfsi_listing_options,
    cb: vfsi_indexed_listdir_cursor_cb,
    userdata: *mut c_void,
    results: *mut vfsi_listing_result,
) -> c_int {
    ffi_guard!(libc::EIO, {
        // SAFETY: non-null pointers satisfy the documented C pointer contract.
        let (Some(fs), Some(options), Some(cb)) =
            (unsafe { fs.as_ref() }, unsafe { options.as_ref() }, cb)
        else {
            return libc::EINVAL;
        };
        if count == 0 {
            return 0;
        }
        if count > VFSI_DIRECTORY_COHORT_MAX as usize
            || dirs.is_null()
            || cursors.is_null()
            || results.is_null()
        {
            return libc::EINVAL;
        }
        // SAFETY: the caller provides count exclusive, aligned output slots.
        let results =
            unsafe { initialized_output_slice(results, count, |_| vfsi_listing_result::default()) };
        let Some(fields) = AttrMask::from_bits(options.attributes) else {
            return libc::EINVAL;
        };
        let fields = fields | AttrMask::MODE;
        if !mask().contains(fields) {
            return libc::ENOTSUP;
        }
        let mut paths = Vec::with_capacity(count);
        let mut originals = Vec::with_capacity(count);
        let mut seen = HashSet::new();
        let mut planning_bytes = 0usize;
        for index in 0..count {
            // SAFETY: both input arrays contain count live elements.
            let (path, cursor) = unsafe { (*dirs.add(index), *cursors.add(index)) };
            if path.is_null() {
                return libc::EINVAL;
            }
            // SAFETY: input strings stay readable and unchanged for the full call.
            let original = unsafe { CStr::from_ptr(path) };
            let path = Path::new(std::ffi::OsStr::from_bytes(original.to_bytes()));
            if original.to_bytes().len()
                > vfsi_sync::DEFAULT_DIRECTORY_MAX_PATH_BYTES.saturating_sub(planning_bytes)
            {
                return libc::EFBIG;
            }
            let mapped = match vpath_for(fs, path) {
                Ok(path) => path,
                Err(error) => return vf_code(&error),
            };
            let Some(total) = planning_bytes
                .checked_add(path.as_os_str().len())
                .and_then(|n| n.checked_add(mapped.as_os_str().len()))
            else {
                return libc::EFBIG;
            };
            if total > vfsi_sync::DEFAULT_DIRECTORY_MAX_PATH_BYTES {
                return libc::EFBIG;
            }
            planning_bytes = total;
            if !cursor.is_null() {
                if !seen.insert(cursor as usize) {
                    return libc::EINVAL;
                }
                // SAFETY: non-null cursor pointers must originate from this API.
                let cursor = unsafe { &*cursor };
                if !Arc::ptr_eq(&cursor.owner, &fs.cursor_owner) || cursor.path != mapped {
                    return libc::EINVAL;
                }
            }
            originals.push(original);
            paths.push(mapped);
        }
        let mut pending = VecDeque::new();
        for index in 0..count {
            // SAFETY: validation established unique, live cursor ownership;
            // each array element is consumed exactly once and cleared.
            let raw = unsafe { std::mem::replace(&mut *cursors.add(index), std::ptr::null_mut()) };
            let state = if raw.is_null() {
                None
            } else {
                // SAFETY: preflight checked unique ownership; the input slot is now null.
                unsafe { Box::from_raw(raw) }.state.take()
            };
            pending.push_back((index, state, raw.is_null()));
        }
        let snapshot = (|| -> Result<Vec<Row>, VfError> {
            let mut backend = lock_or_io(&fs.fs)?;
            let width = backend
                .directory_page_batch_size()
                .clamp(1, VFSI_DIRECTORY_COHORT_MAX as usize);
            let mut rows = Vec::new();
            let mut bytes = 0usize;
            while let Some(&(_, _, follow)) = pending.front() {
                let mut wave = Vec::new();
                while wave.len() < width && pending.front().is_some_and(|(_, _, p)| *p == follow) {
                    wave.push(pending.pop_front().expect("pending front exists"));
                }
                let indices: Vec<_> = wave.iter().map(|(index, _, _)| *index).collect();
                let refs: Vec<_> = indices
                    .iter()
                    .map(|&index| paths[index].as_path())
                    .collect();
                let probe = options
                    .max_entries
                    .saturating_sub(rows.len())
                    .saturating_add(1);
                let page_size = probe.clamp(1, 256);
                let states = wave.into_iter().map(|(_, state, _)| state).collect();
                let pages =
                    backend.vlistdir_pages_impl(&refs, fields, states, page_size, probe, follow)?;
                if pages.len() != indices.len() {
                    return Err(VfError::invalid_reply(
                        None,
                        "invalid cohort directory page count",
                    ));
                }
                for (index, (attrs, next, children)) in indices.into_iter().zip(pages) {
                    if children.len() != attrs.len()
                        || attrs.len() > page_size
                        || (attrs.is_empty() && next.is_some())
                    {
                        return Err(VfError::invalid_reply(
                            Some(index),
                            "invalid cohort directory page progress",
                        ));
                    }
                    for (attrs, child) in attrs.into_iter().zip(children) {
                        let path = attrs.file.path().ok_or_else(|| {
                            VfError::invalid_reply(Some(index), "missing cohort entry path")
                        })?;
                        if path.parent() != Some(paths[index].as_path())
                            || (child.is_some() && attrs.ftype != vfsi_sync::VfType::Directory)
                        {
                            return Err(VfError::invalid_reply(
                                Some(index),
                                "invalid cohort child path or cursor",
                            ));
                        }
                        bytes = bytes
                            .checked_add(path.as_os_str().len())
                            .ok_or_else(|| VfError::errno(index, libc::EFBIG as u32))?;
                        if rows.len() >= options.max_entries || bytes > options.max_path_bytes {
                            return Err(VfError::errno(index, libc::EFBIG as u32));
                        }
                        if !attrs.returned.contains(fields) {
                            return Err(VfError::unsupported(index));
                        }
                        let name = path
                            .file_name()
                            .and_then(|name| cstr_from_os(name.as_bytes()))
                            .ok_or_else(|| {
                                VfError::invalid_reply(Some(index), "invalid cohort entry name")
                            })?;
                        let child = (attrs.ftype == vfsi_sync::VfType::Directory).then(|| {
                            Box::new(vfsi_dir_cursor {
                                owner: Arc::clone(&fs.cursor_owner),
                                path: path.to_path_buf(),
                                state: child,
                            })
                        });
                        rows.push(Row {
                            index,
                            name,
                            attrs: vfsi_attrs::from_vf(&attrs),
                            child,
                        });
                    }
                    if let Some(next) = next {
                        pending.push_back((index, Some(next), follow));
                    }
                }
            }
            Ok(rows)
        })();
        let rows = match snapshot {
            Ok(rows) => rows,
            Err(error) => return vf_code(&error),
        };
        for result in results.iter_mut() {
            result.completion = 1;
        }
        for row in rows {
            results[row.index].delivered += 1;
            let child = row.child.map_or(std::ptr::null_mut(), Box::into_raw);
            // SAFETY: callback inputs live for the invocation. A non-null
            // child transfers unique ownership unconditionally to the callback.
            if !unsafe {
                cb(
                    row.index,
                    originals[row.index].as_ptr(),
                    row.name.as_ptr(),
                    &row.attrs,
                    child,
                    userdata,
                )
            } {
                for result in results.iter_mut() {
                    result.completion = 2;
                }
                break;
            }
        }
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfsi_sync::backend::{HandleBackend, VectorBackend};

    #[derive(Default)]
    struct Rows {
        names: Vec<(usize, Vec<u8>)>,
        children: Vec<(CString, *mut vfsi_dir_cursor)>,
        fs: *mut vfsi_fs,
        stop_after: Option<usize>,
        capabilities: u64,
    }
    impl Drop for Rows {
        fn drop(&mut self) {
            for (_, cursor) in self.children.drain(..) {
                // SAFETY: this fixture uniquely owns the cursor transferred by the callback.
                unsafe { vfsi_dir_cursor_free(cursor) };
            }
        }
    }
    unsafe extern "C" fn collect(
        index: usize,
        dir: *const c_char,
        name: *const c_char,
        _: *const vfsi_attrs,
        child: *mut vfsi_dir_cursor,
        data: *mut c_void,
    ) -> bool {
        // SAFETY: run supplies exclusive Rows storage for the whole callback.
        let rows = unsafe { &mut *data.cast::<Rows>() };
        // SAFETY: run keeps this filesystem handle live during callbacks.
        rows.capabilities = unsafe { vfsi_capabilities(rows.fs) };
        // SAFETY: the listing callback supplies a readable C string for this invocation.
        let name = unsafe { CStr::from_ptr(name) }.to_bytes();
        rows.names.push((index, name.to_vec()));
        if !child.is_null() {
            // SAFETY: the listing callback supplies a readable C string for this invocation.
            let mut path = unsafe { CStr::from_ptr(dir) }.to_bytes().to_vec();
            if !path.ends_with(b"/") {
                path.push(b'/');
            }
            path.extend_from_slice(name);
            rows.children.push((CString::new(path).unwrap(), child));
        }
        rows.stop_after.is_none_or(|stop| rows.names.len() < stop)
    }
    fn options(max_entries: usize, max_path_bytes: usize) -> vfsi_listing_options {
        vfsi_listing_options {
            max_entries,
            max_path_bytes,
            attributes: VFSI_ATTR_MODE | VFSI_ATTR_SIZE,
        }
    }
    fn local(root: &Path) -> *mut vfsi_fs {
        make_fs(
            Box::new(vfsi_posix::backend(root).unwrap()),
            PathBuf::from("/mount"),
            PathBuf::from("/"),
        )
    }
    fn run(
        fs: *mut vfsi_fs,
        paths: &[CString],
        cursors: &mut [*mut vfsi_dir_cursor],
        options: vfsi_listing_options,
        rows: &mut Rows,
    ) -> (c_int, Vec<vfsi_listing_result>) {
        let pointers: Vec<_> = paths.iter().map(|path| path.as_ptr()).collect();
        let mut results = vec![
            vfsi_listing_result {
                completion: 7,
                delivered: 99
            };
            paths.len()
        ];
        rows.fs = fs;
        // SAFETY: the fixture owns the live handle, paths, cursor slots, and result storage.
        let status = unsafe {
            vfsi_listdirs_from_cursors(
                fs,
                pointers.as_ptr(),
                cursors.as_mut_ptr(),
                paths.len(),
                &options,
                Some(collect),
                rows as *mut _ as *mut c_void,
                results.as_mut_ptr(),
            )
        };
        (status, results)
    }
    fn seed(fs: *mut vfsi_fs, path: &str) -> *mut vfsi_dir_cursor {
        Box::into_raw(Box::new(vfsi_dir_cursor {
            // SAFETY: the fixture keeps this filesystem handle live for this borrow.
            owner: Arc::clone(&unsafe { &*fs }.cursor_owner),
            path: PathBuf::from(path),
            state: None,
        }))
    }

    #[test]
    fn cohort_cursors_page_preserve_indices_bytes_and_unlocked_callbacks() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("root/child")).unwrap();
        std::fs::create_dir(root.path().join("empty")).unwrap();
        std::fs::write(root.path().join("root/child/leaf"), b"x").unwrap();
        for i in 0..300 {
            std::fs::write(root.path().join(format!("root/f-{i}")), b"").unwrap();
        }
        std::fs::write(
            root.path()
                .join(OsString::from_vec(b"root/raw-\xff".to_vec())),
            b"",
        )
        .unwrap();
        let fs = local(root.path());
        let mut rows = Rows::default();
        let (status, results) = run(
            fs,
            &[
                CString::new("/mount/root").unwrap(),
                CString::new("/mount/empty").unwrap(),
            ],
            &mut [std::ptr::null_mut(); 2],
            options(400, 1 << 20),
            &mut rows,
        );
        assert_eq!(status, 0);
        assert_eq!(
            results
                .iter()
                .map(|r| (r.completion, r.delivered))
                .collect::<Vec<_>>(),
            [(1, 302), (1, 0)]
        );
        assert!(rows.names.iter().all(|(index, _)| *index == 0));
        assert!(rows.names.iter().any(|(_, name)| name == b"raw-\xff"));
        assert_ne!(rows.capabilities, 0);
        let (path, cursor) = rows.children.pop().unwrap();
        let mut cursors = [cursor];
        let mut children = Rows::default();
        let (status, results) = run(fs, &[path], &mut cursors, options(20, 4096), &mut children);
        assert_eq!(status, 0);
        assert!(cursors[0].is_null());
        assert_eq!(results[0].delivered, 1);
        assert_eq!(children.names, [(0, b"leaf".to_vec())]);
        // SAFETY: the fixture owns these live handles and frees each exactly once.
        unsafe { vfsi_free(fs) };
    }

    #[test]
    fn cohort_rejects_foreign_duplicate_and_misassociated_cursors_without_consumption() {
        let root = tempfile::tempdir().unwrap();
        let first = local(root.path());
        let second = local(root.path());
        let cursor = seed(first, "/child");
        let right = CString::new("/mount/child").unwrap();
        for (fs, paths) in [
            (second, vec![right.clone()]),
            (first, vec![CString::new("/mount/other").unwrap()]),
            (first, vec![right.clone(), right]),
        ] {
            let mut cursors = vec![cursor; paths.len()];
            let mut rows = Rows::default();
            let (status, results) = run(fs, &paths, &mut cursors, options(20, 4096), &mut rows);
            assert_eq!(status, libc::EINVAL);
            assert!(cursors.iter().all(|value| *value == cursor));
            assert!(rows.names.is_empty());
            assert!(results
                .iter()
                .all(|r| r.completion == 0 && r.delivered == 0));
        }
        // SAFETY: the fixture owns these live handles and frees each exactly once.
        unsafe {
            vfsi_dir_cursor_free(cursor);
            vfsi_free(first);
            vfsi_free(second);
        }
    }

    #[test]
    fn late_cohort_quota_failure_delivers_nothing_and_consumes_input_cursors() {
        let root = tempfile::tempdir().unwrap();
        for path in ["a/child", "b/child"] {
            std::fs::create_dir_all(root.path().join(path)).unwrap();
        }
        let fs = local(root.path());
        for limits in [options(1, 4096), options(20, 0)] {
            let mut cursors = [seed(fs, "/a"), seed(fs, "/b")];
            let mut rows = Rows::default();
            let (status, results) = run(
                fs,
                &[
                    CString::new("/mount/a").unwrap(),
                    CString::new("/mount/b").unwrap(),
                ],
                &mut cursors,
                limits,
                &mut rows,
            );
            assert_eq!(status, libc::EFBIG);
            assert!(cursors.iter().all(|cursor| cursor.is_null()));
            assert!(rows.names.is_empty());
            assert!(results
                .iter()
                .all(|r| r.completion == 0 && r.delivered == 0));
            // SAFETY: the fixture keeps this filesystem handle live for this borrow.
            assert_eq!(Arc::strong_count(&unsafe { &*fs }.cursor_owner), 1);
        }
        // SAFETY: the fixture owns these live handles and frees each exactly once.
        unsafe { vfsi_free(fs) };
    }

    #[test]
    fn cohort_stop_marks_empty_roots_stopped_and_drops_undelivered_children() {
        let root = tempfile::tempdir().unwrap();
        for path in ["empty", "a/child", "a/other"] {
            std::fs::create_dir_all(root.path().join(path)).unwrap();
        }
        let fs = local(root.path());
        let mut rows = Rows::default();
        rows.stop_after = Some(1);
        let (status, results) = run(
            fs,
            &[
                CString::new("/mount/empty").unwrap(),
                CString::new("/mount/a").unwrap(),
            ],
            &mut [std::ptr::null_mut(); 2],
            options(20, 4096),
            &mut rows,
        );
        assert_eq!(status, 0);
        assert!(results.iter().all(|r| r.completion == 2));
        assert_eq!(results.iter().map(|r| r.delivered).sum::<usize>(), 1);
        // SAFETY: the fixture keeps this filesystem handle live for this borrow.
        assert_eq!(Arc::strong_count(&unsafe { &*fs }.cursor_owner), 2);
        drop(rows);
        // SAFETY: the fixture keeps this filesystem handle live for this borrow.
        assert_eq!(Arc::strong_count(&unsafe { &*fs }.cursor_owner), 1);
        // SAFETY: the fixture owns these live handles and frees each exactly once.
        unsafe { vfsi_free(fs) };
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cohort_rejects_oversized_planning_before_copying_input_paths() {
        const CHILD: &str = "VFSI_COHORT_PLANNING_LIMIT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "directory::tests::cohort_rejects_oversized_planning_before_copying_input_paths", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                output.status.success(),
                "bounded planning child failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let fs = local(root.path());
        // Exceed glibc's ordinary thread-arena reservation as well as the quota,
        // so the old extra copy needs new address space rather than spare arena bytes.
        let input_len = vfsi_sync::DEFAULT_DIRECTORY_MAX_PATH_BYTES.max(128 * 1024 * 1024) + 8;
        let mut bytes = Vec::with_capacity(input_len + 1);
        bytes.extend_from_slice(b"/mount/");
        bytes.resize(input_len, b'x');
        let path = CString::new(bytes).unwrap();
        let original = seed(fs, "/unchanged");
        let mut cursors = [original];
        let mut old_limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            // SAFETY: old_limit is writable syscall output in this isolated child.
            unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut old_limit) },
            0
        );
        let pages: u64 = std::fs::read_to_string("/proc/self/statm")
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        // SAFETY: sysconf only queries the host page size.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(page_size > 0);
        let limit = libc::rlimit {
            rlim_cur: (pages * page_size as u64 + 16 * 1024 * 1024).min(old_limit.rlim_cur),
            rlim_max: old_limit.rlim_max,
        };
        let no_core = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: only this test child is affected; avoid producing a core on the reproducing OOM path.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) }, 0);
        // SAFETY: the child retains its hard limit and only lowers its address-space soft limit.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) }, 0);
        let mut rows = Rows::default();
        let (status, results) = run(fs, &[path], &mut cursors, options(1, 1024), &mut rows);
        // SAFETY: restore this child's original limit before diagnostics/cleanup.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_AS, &old_limit) }, 0);
        assert_eq!(status, libc::EFBIG);
        assert_eq!(
            cursors[0], original,
            "planning errors must not consume input cursors"
        );
        assert!(rows.names.is_empty());
        assert_eq!(results[0].completion, 0);
        // SAFETY: the fixture still owns this unchanged cursor and live filesystem.
        unsafe {
            vfsi_dir_cursor_free(original);
            vfsi_free(fs);
        }
    }

    #[test]
    #[ignore = "requires a live NFS fixture"]
    fn retained_cohort_children_survive_parent_rename_with_bounded_compounds() {
        let host = std::env::var("VFSI_NFS_SERVER").expect("required live NFS server");
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "2".into())
            .parse()
            .unwrap();
        let mut backend = NfsVecFs::connect_minor(&host, minor).unwrap();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = format!("/vfsi-rsync-cohort-{}-{unique}", std::process::id());
        let moved = format!("{root}-moved");
        for i in 0..12 {
            backend
                .ensure_dir_impl(Path::new(&format!("{root}/d-{i}")), 0o755)
                .unwrap();
            let path = format!("{root}/d-{i}/leaf");
            let file = backend
                .open_raw_impl(Path::new(&path), libc::O_CREAT | libc::O_WRONLY, 0o600)
                .unwrap();
            backend.close_impl(&file).unwrap();
        }
        let fs = make_fs(Box::new(backend), PathBuf::from("/"), PathBuf::from("/"));
        let mut roots = Rows::default();
        let (status, _) = run(
            fs,
            &[CString::new(root.clone()).unwrap()],
            &mut [std::ptr::null_mut()],
            options(100, 1 << 20),
            &mut roots,
        );
        assert_eq!(status, 0);
        assert_eq!(roots.children.len(), 12);
        // SAFETY: the fixture keeps this filesystem handle live for this borrow.
        lock_or_io(&unsafe { &*fs }.fs)
            .unwrap()
            .vrename_impl(&[(
                FileOperand::from_path(&root),
                FileOperand::from_path(&moved),
            )])
            .unwrap();
        let (paths, mut cursors): (Vec<_>, Vec<_>) = roots.children.drain(..).unzip();
        let _ = vfsi_nfs::compound::thread_compound_stats();
        let mut rows = Rows::default();
        let (status, results) = run(fs, &paths, &mut cursors, options(100, 1 << 20), &mut rows);
        let compounds = vfsi_nfs::compound::thread_compound_stats().0;
        // SAFETY: the fixture keeps this filesystem handle live for this borrow.
        let cleanup = lock_or_io(&unsafe { &*fs }.fs)
            .unwrap()
            .remove_paths_impl(&[Path::new(&moved)], true);
        // SAFETY: the fixture owns these live handles and frees each exactly once.
        unsafe { vfsi_free(fs) };
        cleanup.unwrap();
        assert_eq!(status, 0);
        assert!(cursors.iter().all(|cursor| cursor.is_null()));
        assert!(results
            .iter()
            .all(|r| r.completion == 1 && r.delivered == 1));
        assert_eq!(rows.names.len(), 12);
        assert!(
            (1..=2).contains(&compounds),
            "retained child cohort used {compounds} compounds"
        );
    }
}
