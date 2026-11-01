#![allow(clippy::undocumented_unsafe_blocks, unsafe_op_in_unsafe_fn)]
use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use vfsi_sync::backend::{HandleBackend, VectorBackend};

fn local(root: &Path, mount: &str) -> *mut vfsi_fs {
    let root = CString::new(root.as_os_str().as_bytes()).unwrap();
    let mount = CString::new(mount).unwrap();
    let mut fs = std::ptr::null_mut();
    assert_eq!(
        unsafe { vfsi_dummy_open_mount(root.as_ptr(), mount.as_ptr(), &mut fs) },
        0
    );
    fs
}
fn query(fs: *mut vfsi_fs, path: &str, flags: u32) -> vfsi_object_id {
    let path = CString::new(path).unwrap();
    let mut out = vfsi_object_id::empty();
    let result = unsafe { vfsi_get_object_id(fs, path.as_ptr(), flags, &mut out) };
    assert_eq!(result.category, VFSI_ERROR_NONE, "{:?}", result.message);
    assert_ne!(out.namespace_id, 0);
    out
}

#[test]
fn hardlinks_mount_views_nofollow_and_retained_rename_use_complete_identity() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("original"), b"original").unwrap();
    std::fs::hard_link(root.path().join("original"), root.path().join("alias")).unwrap();
    std::os::unix::fs::symlink("original", root.path().join("symlink")).unwrap();
    let first = local(root.path(), "/view-a");
    let second = local(root.path(), "/view-b");
    let id = query(first, "/view-a/original", 0);
    assert_eq!(id, query(first, "/view-a/alias", 0));
    assert_eq!(id, query(second, "/view-b/original", 0));
    assert_eq!(id, query(first, "/view-a/symlink", 0));
    assert_ne!(id, query(first, "/view-a/symlink", VFSI_OBJECT_ID_NOFOLLOW));
    let path = c"/view-a/original";
    let fd = unsafe { vfsi_open(first, path.as_ptr(), libc::O_RDONLY, 0) };
    assert!(fd >= 0);
    std::fs::rename(root.path().join("original"), root.path().join("renamed")).unwrap();
    std::fs::write(root.path().join("original"), b"replacement").unwrap();
    let mut retained = vfsi_object_id::empty();
    assert_eq!(
        unsafe { vfsi_fget_object_id(first, fd, &mut retained) }.category,
        0
    );
    assert_eq!(retained, id);
    assert_eq!(id, query(second, "/view-b/renamed", 0));
    assert_ne!(id, query(first, "/view-a/original", 0));
    assert_eq!(unsafe { vfsi_close(first, fd) }, 0);
    retained.namespace_id = 999;
    let error = unsafe { vfsi_fget_object_id(first, fd, &mut retained) };
    assert_eq!(error.err_no, libc::EBADF as u32);
    unsafe {
        vfsi_free(first);
        vfsi_free(second);
    }
}

struct IdentityBackend {
    namespace: Option<&'static str>,
    calls: Arc<AtomicUsize>,
    missing: AttrMask,
    wrong_target: bool,
    failure: bool,
}
impl HandleBackend for IdentityBackend {
    fn sync_data(&mut self, _: &FileOperand) -> Result<(), VfError> {
        unreachable!()
    }
    fn open_impl(&mut self, _: &vfsi_sync::OpenOp) -> Result<FileOperand, VfError> {
        unreachable!()
    }
    fn close_impl(&mut self, _: &FileOperand) -> Result<(), VfError> {
        Ok(())
    }
    fn read_impl(&mut self, _: &ReadOp) -> Result<vfsi_sync::ReadResult, VfError> {
        unreachable!()
    }
    fn write_impl(
        &mut self,
        _: WriteOp<&FileOperand, &[u8]>,
    ) -> Result<vfsi_sync::WriteResult, VfError> {
        unreachable!()
    }
    fn seek_impl(&mut self, _: &FileOperand, _: std::io::SeekFrom) -> Result<u64, VfError> {
        unreachable!()
    }
    fn metadata_impl(
        &mut self,
        _: vfsi_sync::Target<'_, FileOperand>,
        _: vfsi_sync::api::AttrsOptions,
    ) -> Result<VfAttrs, VfError> {
        unreachable!()
    }
    fn set_attributes_impl(
        &mut self,
        _: &vfsi_sync::SetAttrsOp<vfsi_sync::Target<'_, FileOperand>>,
    ) -> Result<(), VfError> {
        unreachable!()
    }
}
impl VectorBackend for IdentityBackend {
    fn vread_impl(&mut self, _: &[ReadOp]) -> Result<Vec<vfsi_sync::ReadResult>, VfError> {
        unreachable!()
    }
    fn vgetattrs_impl(&mut self, attrs: &mut [VfAttrs]) -> Result<(), VfError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.failure {
            return Err(VfError::transport_with_kind(
                Some(1),
                vfsi_sync::TransportKind::Connection,
                "injected identity disconnect",
            ));
        }
        for attrs in attrs {
            attrs.returned = attrs.masks & !self.missing;
            attrs.fs_id = (1u128 << 100) | 9;
            attrs.fileid = 0; // A real zero file number must not mean absent.
            attrs.ftype = vfsi_sync::VfType::Regular;
            attrs.mode = 0o100644;
            if self.wrong_target {
                attrs.file = FileOperand::from_path("/wrong");
            }
        }
        Ok(())
    }
    fn vgetattrs_nofollow_impl(&mut self, attrs: &mut [VfAttrs]) -> Result<(), VfError> {
        self.vgetattrs_impl(attrs)
    }
}
impl BindingBackend for IdentityBackend {
    fn identity_namespace(&self) -> Option<std::borrow::Cow<'_, str>> {
        self.namespace.map(std::borrow::Cow::Borrowed)
    }
}
fn fake(missing: AttrMask, wrong_target: bool, failure: bool) -> (*mut vfsi_fs, Arc<AtomicUsize>) {
    scoped_fake(None, missing, wrong_target, failure)
}
fn scoped_fake(
    namespace: Option<&'static str>,
    missing: AttrMask,
    wrong_target: bool,
    failure: bool,
) -> (*mut vfsi_fs, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let fs = make_fs(
        Box::new(IdentityBackend {
            namespace,
            calls: calls.clone(),
            missing,
            wrong_target,
            failure,
        }),
        "/".into(),
        "/".into(),
    );
    (fs, calls)
}

#[test]
fn namespace_collisions_zero_numbers_and_vector_batching_are_not_fabricated() {
    let (first, calls) = scoped_fake(Some("nfs:test-server-a"), AttrMask::empty(), false, false);
    let (alias, _) = scoped_fake(Some("nfs:test-server-a"), AttrMask::empty(), false, false);
    let (other, _) = scoped_fake(Some("nfs:test-server-b"), AttrMask::empty(), false, false);
    let a = query(first, "/a", 0);
    let b = query(other, "/a", 0);
    assert_eq!(a.fs_id, b.fs_id);
    assert_eq!(a.fileid, b.fileid);
    assert_ne!(a.namespace_id, b.namespace_id);
    assert_eq!(a, query(alias, "/same-server-view", 0));
    assert_eq!(a.fs_id, [1 << 36, 9]);
    assert_eq!(a.fileid, 0);
    let mut ids = [vfsi_object_id::empty(); 4];
    let mut stat: vfsi_attrs = unsafe { std::mem::zeroed() };
    let ops: Vec<_> = ids
        .iter_mut()
        .enumerate()
        .map(|(index, id)| vfsi_object_id_op {
            path: c"/alias".as_ptr(),
            fd: -1,
            flags: 0,
            out: id,
            attrs: if index == 0 {
                &mut stat
            } else {
                std::ptr::null_mut()
            },
        })
        .collect();
    let mut results = [vfsi_result::panic(); 4];
    let before = calls.load(Ordering::Relaxed);
    let result =
        unsafe { vfsi_get_object_idv(first, ops.as_ptr(), ops.len(), results.as_mut_ptr()) };
    assert_eq!((result.category, result.index), (0, 4));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        before + 1,
        "one GETATTR vector, not per-file calls"
    );
    assert!(ids.iter().all(|id| *id == a));
    assert!(results.iter().all(|item| item.category == 0));
    assert_eq!(stat.fileid, a.fileid);
    assert_eq!(stat.abi_version, 3);
    unsafe {
        vfsi_free(first);
        vfsi_free(alias);
        vfsi_free(other);
    }
    let (unknown_a, _) = fake(AttrMask::empty(), false, false);
    let (unknown_b, _) = fake(AttrMask::empty(), false, false);
    assert_ne!(query(unknown_a, "/a", 0), query(unknown_b, "/a", 0));
    unsafe {
        vfsi_free(unknown_a);
        vfsi_free(unknown_b);
    }
}

#[test]
fn identity_preflight_rejects_short_headers_and_wrong_versions_without_io() {
    let (fs, calls) = fake(AttrMask::empty(), false, false);
    #[repr(align(8))]
    struct Tiny([u32; 3]);
    let mut tiny = Tiny([8u32, VFSI_OBJECT_ID_VERSION, 0xdeadbeef]);
    let error = unsafe { vfsi_get_object_id(fs, c"/a".as_ptr(), 0, tiny.0.as_mut_ptr().cast()) };
    assert_eq!(error.category, VFSI_ERROR_INVALID_ARGUMENT);
    assert_eq!(tiny.0, [8, VFSI_OBJECT_ID_VERSION, 0xdeadbeef]);
    let mut ids = [vfsi_object_id::empty(); 2];
    ids[0].namespace_id = 999;
    ids[1].version += 1;
    let ops: Vec<_> = ids
        .iter_mut()
        .map(|id| vfsi_object_id_op {
            path: c"/a".as_ptr(),
            fd: -1,
            flags: 0,
            out: id,
            attrs: std::ptr::null_mut(),
        })
        .collect();
    let mut results = [vfsi_result::panic(); 2];
    let error = unsafe { vfsi_get_object_idv(fs, ops.as_ptr(), 2, results.as_mut_ptr()) };
    assert_eq!(error.index, 1);
    assert_eq!(results[0].category, VFSI_ERROR_NOT_ATTEMPTED);
    assert_eq!(results[1].category, VFSI_ERROR_INVALID_ARGUMENT);
    assert_eq!(
        ids[0].namespace_id, 999,
        "preflight must not touch another output"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    unsafe {
        vfsi_free(fs);
    }
}

#[test]
fn missing_or_malformed_identity_and_transport_failures_clear_outputs_without_replay() {
    for (missing, wrong_target, failure) in [
        (AttrMask::FS_ID, false, false),
        (AttrMask::FILEID, false, false),
        (AttrMask::empty(), true, false),
        (AttrMask::empty(), false, true),
    ] {
        let (fs, calls) = fake(missing, wrong_target, failure);
        let mut ids = [vfsi_object_id::empty(); 2];
        for id in &mut ids {
            id.namespace_id = 999;
        }
        let ops: Vec<_> = ids
            .iter_mut()
            .map(|id| vfsi_object_id_op {
                path: c"/a".as_ptr(),
                fd: -1,
                flags: 0,
                out: id,
                attrs: std::ptr::null_mut(),
            })
            .collect();
        let mut results = [vfsi_result::panic(); 2];
        let result = unsafe { vfsi_get_object_idv(fs, ops.as_ptr(), 2, results.as_mut_ptr()) };
        assert_ne!(result.category, 0);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(ids.iter().all(|id| id.namespace_id == 0));
        if !missing.is_empty() {
            assert_eq!(result.category, VFSI_ERROR_UNSUPPORTED);
        } else {
            assert_eq!(result.category, VFSI_ERROR_TRANSPORT);
        }
        if failure {
            assert_eq!(result.index, 1);
            assert_eq!(results[0].category, VFSI_ERROR_INDETERMINATE);
        }
        unsafe {
            vfsi_free(fs);
        }
    }
}

#[test]
fn abi_v3_attributes_and_future_output_tail_are_preserved() {
    assert_eq!(VFSI_ABI_VERSION, 3);
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(std::mem::size_of::<vfsi_attrs>(), 104);
        assert_eq!(std::mem::offset_of!(vfsi_attrs, fileid), 32);
        assert_eq!(std::mem::offset_of!(vfsi_attrs, ctime_nsec), 96);
    }
    #[repr(C)]
    struct Future {
        id: vfsi_object_id,
        tail: u64,
    }
    let (fs, _) = fake(AttrMask::empty(), false, false);
    let mut output = Future {
        id: vfsi_object_id::empty(),
        tail: 0x123456789abcdef0,
    };
    output.id.struct_size = std::mem::size_of::<Future>() as u32;
    assert_eq!(
        unsafe { vfsi_get_object_id(fs, c"/a".as_ptr(), 0, &mut output.id) }.category,
        0
    );
    assert_eq!(output.tail, 0x123456789abcdef0);
    assert_eq!(
        output.id.struct_size,
        std::mem::size_of::<vfsi_object_id>() as u32
    );
    unsafe {
        vfsi_free(fs);
    }
}

#[test]
#[ignore = "requires writable NFSv4 server; run explicitly with VFSI_NFS_SERVER"]
fn live_nfs_retained_identity_survives_rename_replacement_and_reconnect() {
    let host = std::env::var("VFSI_NFS_SERVER").expect("set VFSI_NFS_SERVER");
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let backend = NfsVecFs::connect_minor(&host, minor).unwrap();
    let fs = make_fs(Box::new(backend), "/".into(), "/".into());
    let alias = make_fs(
        Box::new(NfsVecFs::connect_minor(&host, minor).unwrap()),
        "/mount-alias".into(),
        "/".into(),
    );
    let name = format!("/vfsi-c-id-{}-{minor}", std::process::id());
    let saved = format!("{name}-saved");
    let path = CString::new(name.as_str()).unwrap();
    let renamed = CString::new(saved.as_str()).unwrap();
    let fd = unsafe {
        vfsi_open(
            fs,
            path.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600,
        )
    };
    assert!(fd >= 0, "open failed: {fd}");
    let original = query(fs, &name, 0);
    assert_eq!(original, query(alias, &format!("/mount-alias{name}"), 0));
    assert_eq!(
        unsafe { vfsi_rename(fs, path.as_ptr(), renamed.as_ptr()) },
        0
    );
    let replacement = unsafe {
        vfsi_open(
            fs,
            path.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600,
        )
    };
    assert!(replacement >= 0);
    assert_ne!(original, query(fs, &name, 0));
    unsafe {
        (*fs).fs.lock().unwrap().reconnect_for_test().unwrap();
    }
    let mut retained = vfsi_object_id::empty();
    assert_eq!(
        unsafe { vfsi_fget_object_id(fs, fd, &mut retained) }.category,
        0
    );
    assert_eq!(retained, original);
    assert_eq!(query(fs, &saved, 0), original);
    assert_eq!(query(alias, &format!("/mount-alias{saved}"), 0), original);
    assert_ne!(query(fs, &name, 0), original);
    assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);
    assert_eq!(unsafe { vfsi_close(fs, replacement) }, 0);
    assert_eq!(unsafe { vfsi_remove(fs, path.as_ptr()) }, 0);
    assert_eq!(unsafe { vfsi_remove(fs, renamed.as_ptr()) }, 0);
    unsafe {
        vfsi_free(fs);
        vfsi_free(alias);
    }
}
