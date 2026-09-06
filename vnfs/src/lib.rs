//! vnfs: a protocol-neutral vectorized filesystem library.
//!
//! - [`vecfs`]: the backend-agnostic vectorized filesystem API ([`VecFs`])
//!   and its shared types.
//! - [`nfs`]: the NFSv4.1 implementation ([`NfsVecFs`]).
//! - [`smb`]: the SMB2/3 implementation ([`SmbVecFs`]) for Samba and other
//!   modern SMB servers.
//! - [`dummy_vecfs`]: a `std::fs`-backed implementation ([`DummyVecFs`]) so
//!   the API also works on non-NFS filesystems.
//! - [`client`]: low-level NFSv4.1 operations (open/read/write/mkdir/...).

#[cfg(feature = "nfs")]
pub mod client;
#[cfg(feature = "nfs")]
pub mod compound;
#[cfg(feature = "dummy")]
pub mod dummy_vecfs;
pub mod error;
#[cfg(feature = "nfs")]
pub mod nfs;
mod path;
#[cfg(feature = "nfs")]
pub mod rpc;
#[cfg(feature = "nfs")]
pub mod session;
#[cfg(feature = "smb")]
pub mod smb;
pub mod vecfs;

#[cfg(feature = "dummy")]
pub use dummy_vecfs::DummyVecFs;
#[cfg(feature = "nfs")]
pub use nfs::NfsVecFs;
#[cfg(feature = "smb")]
pub use smb::SmbVecFs;
pub use vecfs::{
    Adb, AttrMask, ERR_ACCES, ERR_EBADF, ERR_EXIST, ERR_INVAL, ERR_ISDIR, ERR_NOENT, ERR_NOTDIR,
    ExtentPair, Fd, ReadOp, ReadResult, ReadStreamCallback, SeekFrom, VF_CAP_HARDLINKS,
    VF_CAP_LSTAT, VF_CAP_NON_UTF8_PATHS, VF_CAP_POSIX_METADATA, VF_CAP_SERVER_COPY,
    VF_CAP_SYMLINKS, VF_CAP_UNIX_SEMANTICS, VF_ERR_RPC, VF_ERR_UNSUPPORTED, VecFs, VecFsExt,
    VfAttrs, VfError, VfFile, VfOffset, VfPathBase, VfRes, VfResult, VfType, WalkEntry, WriteOp,
    WriteResult,
};
