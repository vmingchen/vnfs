//! vnfs: a vectorized NFSv4.1 client library built on libntirpc.
//!
//! - [`client`]: low-level NFSv4.1 operations (open/read/write/mkdir/...).
//! - [`tc`]: high-level client API mirroring the transactional-compound
//!   `tc_api.h` ([`TxnClient`]).

pub mod client;
pub mod compound;
pub mod error;
pub mod rpc;
pub mod session;
pub mod tc;

pub use tc::TxnClient;
