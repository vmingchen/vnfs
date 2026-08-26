//! vnfs: a vectorized NFSv4.1 client library built on libntirpc.
//!
//! - [`vecfs`]: the backend-agnostic vectorized filesystem API ([`VecFs`])
//!   and its shared types.
//! - [`nfs`]: the NFSv4.1 implementation ([`NfsVecFs`]).
//! - [`dummy_vecfs`]: a `std::fs`-backed implementation ([`DummyVecFs`]) so
//!   the API also works on non-NFS filesystems.
//! - [`client`]: low-level NFSv4.1 operations (open/read/write/mkdir/...).

pub mod client;
pub mod compound;
pub mod dummy_vecfs;
pub mod error;
pub mod nfs;
pub mod rpc;
pub mod session;
pub mod vecfs;

pub use dummy_vecfs::DummyVecFs;
pub use nfs::NfsVecFs;
pub use vecfs::{
    Adb, AttrMask, ExtentPair, VecFs, VfAttrs, VfError, VfFile, VfIoVec, VfPathBase, VfRes,
    VfResult, VfType, WalkEntry,
};
