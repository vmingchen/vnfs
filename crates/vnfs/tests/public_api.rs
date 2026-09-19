//! Compile-time coverage for the public API retained by the `vnfs` facade.
//!
//! These imports deliberately use the historical crate-root paths. Moving the
//! implementation into workspace crates must not make existing callers change
//! their imports.

use vnfs::{
    AttrMask, ExtentPair, Fd, ReadOp, ReadResult, SeekFrom, VecFs, VecFsExt, VfAttrs, VfError,
    VfFile, VfOffset, VfPathBase, VfRes, VfResult, VfType, WalkEntry, WriteOp, WriteResult,
};

fn accepts_vecfs(_: &mut dyn VecFs) {}

fn accepts_vecfs_ext<T: VecFsExt + ?Sized>(_: &mut T) {}

fn accepts_sfsi(_: &mut dyn vnfs::sfsi::VecFs) {}

fn accepts_vfsi(_: &mut dyn vnfs::vfsi::VecFs) {}

#[test]
fn legacy_crate_root_exports_remain_available() {
    let _ = std::mem::size_of::<(
        AttrMask,
        ExtentPair,
        Fd,
        ReadOp,
        ReadResult,
        SeekFrom,
        VfAttrs,
        VfError,
        VfFile,
        VfOffset,
        VfPathBase,
        VfRes,
        VfResult<()>,
        VfType,
        WalkEntry,
        WriteOp,
        WriteResult,
    )>();
    let _: fn(&mut dyn VecFs) = accepts_vecfs;
    let _ = accepts_vecfs_ext::<dyn VecFs>;
    let _: fn(&mut dyn vnfs::sfsi::VecFs) = accepts_sfsi;
    let _: fn(&mut dyn vnfs::vfsi::VecFs) = accepts_vfsi;

    #[cfg(feature = "dummy")]
    {
        let root = std::env::temp_dir().join("vnfs-public-api-compile-test");
        let mut fs = vnfs::DummyVecFs::new(root);
        accepts_vecfs(&mut fs);
        accepts_vecfs_ext(&mut fs);
        accepts_sfsi(&mut fs);
        accepts_vfsi(&mut fs);
    }
}

#[test]
fn rust_native_api_is_curated_and_typed() {
    use vnfs::{
        Capabilities, CopyFileSystem, DirEntry, DirectoryFileSystem, FileSystem, FsClient,
        LinkFileSystem, Metadata, MetadataFileSystem, MetadataQuery, NamespaceFileSystem,
        NativeFileSystem, OpenFlags, OpenOptions, OpenRequest, Permissions, SetAttributes,
        VectorFileSystem, WriteOpRef,
    };
    let _ = std::mem::size_of::<(
        Capabilities,
        OpenFlags,
        OpenRequest,
        MetadataQuery,
        SetAttributes,
        WriteOpRef<'static>,
        FsClient<()>,
        Metadata,
        Permissions,
        DirEntry,
    )>();
    fn accepts_native_scalar<T: FileSystem + ?Sized>(_: &mut T) {}
    fn accepts_native_vector<T: VectorFileSystem + ?Sized>(_: &mut T) {}
    let _ = accepts_native_scalar::<dyn FileSystem>;
    let _ = accepts_native_vector::<dyn VectorFileSystem>;
    fn accepts_metadata<T: MetadataFileSystem + ?Sized>(_: &mut T) {}
    fn accepts_directory<T: DirectoryFileSystem + ?Sized>(_: &mut T) {}
    fn accepts_namespace<T: NamespaceFileSystem + ?Sized>(_: &mut T) {}
    fn accepts_links<T: LinkFileSystem + ?Sized>(_: &mut T) {}
    fn accepts_copy<T: CopyFileSystem + ?Sized>(_: &mut T) {}
    let _ = accepts_metadata::<dyn MetadataFileSystem>;
    let _ = accepts_directory::<dyn DirectoryFileSystem>;
    let _ = accepts_namespace::<dyn NamespaceFileSystem>;
    let _ = accepts_links::<dyn LinkFileSystem>;
    let _ = accepts_copy::<dyn CopyFileSystem>;
    fn accepts_complete_native<T: NativeFileSystem + ?Sized>(_: &mut T) {}
    let _ = accepts_complete_native::<dyn NativeFileSystem>;
    #[cfg(feature = "dummy")]
    {
        let root = std::env::temp_dir().join("vnfs-native-public-api-test");
        let mut fs = vnfs::DummyVecFs::new(root);
        accepts_native_scalar(&mut fs);
        accepts_native_vector(&mut fs);
        let client = FsClient::new(fs);
        let _: OpenOptions<'_, vnfs::DummyVecFs> = client.open_options();
    }
}

#[cfg(feature = "nfs")]
#[test]
fn legacy_nfs_constructor_signatures_remain_available() {
    let _: fn(&str) -> VfResult<vnfs::NfsVecFs> = vnfs::NfsVecFs::connect;
    let _: fn(&str, u32) -> VfResult<vnfs::NfsVecFs> = vnfs::NfsVecFs::connect_minor;
    let _builder: vnfs::NfsBuilder = vnfs::Nfs::builder("server.example.com").root("/export");
}
