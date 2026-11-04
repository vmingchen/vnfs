//! SMB2/3 [`VectorBackend`] backend.
//!
//! The backend talks to Samba (or any modern SMB server) with the pure-Rust
//! `smb2` crate. Path-based stat, rename, delete, and small whole-file I/O use
//! SMB related compounds. Large transfers use the server's negotiated I/O
//! limits, and server-side copies use `FSCTL_SRV_COPYCHUNK` with an automatic
//! read/write fallback.
//! Independent small path operations in a vector call are multiplexed on the
//! authenticated connection; stateful descriptor I/O and dependent mutations
//! stay ordered.
//!
//! SMB paths are Unicode. A `Path` containing non-UTF-8 bytes is rejected with
//! `EILSEQ`; characters which are illegal in SMB names are reversibly mapped
//! by the `smb2` crate.
//!
//! SMB creation uses the server/share security descriptor for the conventional
//! POSIX creation modes (`0666` for files and `0777` for directories); the
//! backend does not translate those modes into a Windows ACL. Other requested
//! creation modes return `Unsupported`. The `MODE` attribute is likewise a
//! conventional synthetic value (`0666` or `0777`), not a report of the
//! server's effective ACL permissions.
//!
//! DFS namespaces/referrals are unsupported. Connection setup rejects DFS
//! shares, and path referral responses return `Unsupported` without following
//! them. Connect directly to the destination server/share instead.

#[doc(hidden)]
mod path {
    pub use vfsi_core::path::*;
}

#[doc(hidden)]
mod vecfs {
    pub use vfsi_sync::*;
}

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
#[cfg(feature = "test-faults")]
use std::sync::Arc;
use std::time::Duration;
use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::*;

use futures_util::future::join_all;
use smb2::client::{ClientConfig, CompoundOp, Connection, SmbClient, Tree};
use smb2::msg::close::CloseRequest;
use smb2::msg::create::{
    CreateDisposition, CreateRequest, CreateResponse, ImpersonationLevel, ShareAccess,
};
use smb2::msg::flush::FlushRequest;
use smb2::msg::query_info::{QueryInfoRequest, QueryInfoResponse};
use smb2::msg::read::{ReadRequest, ReadResponse, SMB2_CHANNEL_NONE};
use smb2::msg::set_info::{InfoType, SetInfoRequest};
use smb2::msg::write::{SMB2_WRITEFLAG_WRITE_THROUGH, WriteRequest, WriteResponse};
use smb2::pack::{FileTime, ReadCursor, Unpack};
use smb2::types::flags::FileAccessMask;
use smb2::types::status::NtStatus;
use smb2::types::{Command, CreditCharge, Dialect, FileId, OplockLevel, TreeId};
use smb2::{Error as SmbError, ErrorKind as SmbErrorKind};
use tokio::runtime::{Builder, Runtime};
use vfsi_core::internal::ManyResults;
#[cfg(feature = "test-faults")]
use vfsi_core::internal::faults::{FaultInjector, OpenFaultPoint};

use crate::path::{normalize_bytes, path_bytes, path_from_bytes};
use crate::vecfs::{
    Adb, AttrMask, CopyOptions, ERR_EBADF, ERR_NOTDIR, ExtentPair, Fd, FileOperand, ReadOp,
    ReadResult, SeekFrom, SetAttrsOp, Target, VfAttrs, VfError, VfOffset, VfPathBase, VfRes,
    VfResult, VfType, WriteOp, WriteResult,
};

const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const FILE_BASIC_INFORMATION: u8 = 4;
const FILE_INTERNAL_INFORMATION: u8 = 6;
const FILE_DISPOSITION_INFORMATION: u8 = 13;
const FILE_END_OF_FILE_INFORMATION: u8 = 20;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const SMB_REPARSE_PROBE_CONCURRENCY: usize = 32;

#[bitfields::bitfield(u8)]
#[derive(PartialEq, Eq)]
struct SmbConnectFlags {
    #[bits(default = true)]
    auto_reconnect: bool,
    #[bits(default = true)]
    compression: bool,
    #[bits(6)]
    _reserved: u8,
}

/// Connection and per-request deadlines for [`SmbVecFs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmbConnectOptions {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    flags: SmbConnectFlags,
}

impl Default for SmbConnectOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            flags: SmbConnectFlags::new(),
        }
    }
}

impl SmbConnectOptions {
    /// Enable read-only recovery after transport failures.
    pub fn with_auto_reconnect(mut self, value: bool) -> Self {
        self.flags.set_auto_reconnect(value);
        self
    }
    /// Enable negotiated SMB compression.
    pub fn with_compression(mut self, value: bool) -> Self {
        self.flags.set_compression(value);
        self
    }
    pub const fn auto_reconnect(self) -> bool {
        self.flags.auto_reconnect()
    }
    pub const fn compression(self) -> bool {
        self.flags.compression()
    }
}

fn check_share_routing(is_dfs: bool, share: &str) -> VfRes {
    if is_dfs {
        return Err(VfError::unsupported(0).with_context(
            "SMB DFS referrals are unsupported; connect directly to the destination server/share",
            share,
        ));
    }
    Ok(())
}

fn checked_offset(base: u64, delta: u64, index: usize) -> VfResult<u64> {
    base.checked_add(delta)
        .ok_or_else(|| VfError::errno(index, libc::EOVERFLOW as u32))
}

#[derive(Debug, Clone)]
struct SmbOpen {
    file_id: FileId,
    path: PathBuf,
    cur_offset: u64,
    append: bool,
    readable: bool,
    writable: bool,
}

/// A synchronous vectorized filesystem client backed by an SMB2/3 share.
///
/// `server` may be `host`, `host:port`, or `[ipv6]:port`; port 445 is added
/// when no port is present. An empty username/password requests guest access.
pub struct SmbVecFs {
    runtime: Runtime,
    client: SmbClient,
    tree: Tree,
    cwd: PathBuf,
    next_fd: Fd,
    open_files: HashMap<Fd, SmbOpen>,
    deferred_closes: Vec<FileId>,
    server_copy_enabled: bool,
    disconnected: bool,
    #[cfg(feature = "test-faults")]
    fault_injector: Option<Arc<dyn FaultInjector>>,
    #[cfg(feature = "test-faults")]
    last_writev_was_concurrent: bool,
}

// Synchronous callers poll request futures themselves; one worker drives the
// connection's background protocol tasks between calls. Retain per-connection
// ownership so shutdown and Python's post-fork reconnect need no global reactor.
fn smb_runtime() -> VfResult<Runtime> {
    Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|error| VfError::transport(None, format!("create SMB runtime: {error}")))
}

impl Drop for SmbVecFs {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

mod connection;
mod copy;
mod directory;
mod handles;
mod io;
mod metadata;
mod protocol;
mod vector;

use protocol::*;

#[cfg(test)]
use vfsi_core::ERR_INVAL;
#[cfg(test)]
mod error_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod option_layout_tests {
    use super::*;
    #[test]
    fn packed_smb_options_keep_defaults_and_flags_independent() {
        assert_eq!(std::mem::size_of::<SmbConnectFlags>(), 1);
        let default = SmbConnectOptions::default();
        assert!(default.auto_reconnect() && default.compression());
        for bits in 0..4 {
            let options = default
                .with_auto_reconnect(bits & 1 != 0)
                .with_compression(bits & 2 != 0);
            assert_eq!(options.auto_reconnect(), bits & 1 != 0);
            assert_eq!(options.compression(), bits & 2 != 0);
            assert_eq!(options.connect_timeout, default.connect_timeout);
            assert_eq!(options.request_timeout, default.request_timeout);
        }
    }
}
