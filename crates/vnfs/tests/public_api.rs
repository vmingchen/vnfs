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

#[cfg(feature = "nfs")]
#[test]
fn legacy_nfs_constructor_signatures_remain_available() {
    let _: fn(&str) -> VfResult<vnfs::NfsVecFs> = vnfs::NfsVecFs::connect;
    let _: fn(&str, u32) -> VfResult<vnfs::NfsVecFs> = vnfs::NfsVecFs::connect_minor;
}

#[cfg(feature = "smb")]
#[test]
fn legacy_smb_constructor_signature_remains_available() {
    let _: fn(&str, &str, &str, &str, &str) -> VfResult<vnfs::SmbVecFs> = vnfs::SmbVecFs::connect;
}
