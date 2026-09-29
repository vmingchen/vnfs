//! Application-facing NFS constructor and concrete client aliases.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::{FsClient, FsFile, VfResult};
use vfsi_nfs::{
    NfsAuthentication, NfsClientBuilder, NfsObserver, NfsReadPool, NfsReadPoolOptions,
    NfsRecoveryPolicy, NfsVecFs,
};

pub type NfsClient = FsClient<NfsVecFs>;
pub type NfsFile = FsFile<NfsVecFs>;

/// Independent NFS sessions for caller-distributed parallel workloads.
/// Cloning a single [`NfsClient`] shares one lock; pool members do not.
#[derive(Clone, Debug)]
pub struct NfsClientPool {
    inner: Arc<NfsClientPoolInner>,
}

#[derive(Debug)]
struct NfsClientPoolInner {
    clients: Vec<NfsClient>,
    next: AtomicUsize,
}

impl NfsClientPool {
    pub fn len(&self) -> usize {
        self.inner.clients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.clients.is_empty()
    }

    /// Return a specific member; cloned handles share that member's session.
    pub fn client(&self, index: usize) -> Option<NfsClient> {
        self.inner.clients.get(index).cloned()
    }

    /// Choose the next independent session in round-robin order.
    pub fn next_client(&self) -> NfsClient {
        let index = self.inner.next.fetch_add(1, Ordering::Relaxed) % self.len();
        self.inner.clients[index].clone()
    }
}

/// Entry point for the Rust-native NFS API.
#[derive(Debug, Clone, Copy, Default)]
pub struct Nfs;

impl Nfs {
    pub fn builder(host: impl Into<String>) -> NfsBuilder {
        NfsBuilder {
            inner: NfsClientBuilder::new(host),
        }
    }

    pub fn connect(host: impl Into<String>) -> VfResult<NfsClient> {
        Self::builder(host).connect()
    }
}

/// NFS configuration builder whose `connect` returns an owned native client.
#[derive(Debug, Clone)]
pub struct NfsBuilder {
    inner: NfsClientBuilder,
}

impl NfsBuilder {
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.inner = self.inner.root(root);
        self
    }

    pub fn minor_version(mut self, version: Option<u32>) -> Self {
        self.inner = self.inner.minor_version(version);
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.inner = self.inner.connect_timeout(timeout);
        self
    }

    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.inner = self.inner.request_timeout(timeout);
        self
    }

    pub fn auth(mut self, auth: NfsAuthentication) -> Self {
        self.inner = self.inner.authentication(auth);
        self
    }

    pub fn client_owner(mut self, owner: impl Into<Vec<u8>>) -> Self {
        self.inner = self.inner.client_owner(owner);
        self
    }

    pub fn recovery_policy(mut self, policy: NfsRecoveryPolicy) -> Self {
        self.inner = self.inner.recovery_policy(policy);
        self
    }

    pub fn auto_reconnect(mut self, enabled: bool) -> Self {
        self.inner = self.inner.auto_reconnect(enabled);
        self
    }

    pub fn max_compound_bytes(mut self, bytes: usize) -> Self {
        self.inner = self.inner.max_compound_bytes(bytes);
        self
    }

    pub fn observer(mut self, observer: Arc<dyn NfsObserver>) -> Self {
        self.inner = self.inner.observer(observer);
        self
    }

    pub fn connect(self) -> VfResult<NfsClient> {
        self.inner.connect().map(FsClient::new)
    }

    pub fn connect_backend(self) -> VfResult<NfsVecFs> {
        self.inner.connect()
    }

    /// Connect 1–64 independent clients. Distribute separate vector cohorts
    /// across members; one vector call itself remains on one session.
    pub fn connect_pool(self, size: usize) -> VfResult<NfsClientPool> {
        let clients = self
            .inner
            .connect_pool(size)?
            .into_iter()
            .map(FsClient::new)
            .collect();
        Ok(NfsClientPool {
            inner: Arc::new(NfsClientPoolInner {
                clients,
                next: AtomicUsize::new(0),
            }),
        })
    }

    /// Connect a reusable bounded pool for ordered pipelined large-file reads.
    pub fn connect_read_pool(self, options: NfsReadPoolOptions) -> VfResult<NfsReadPool> {
        self.inner.connect_read_pool(options)
    }
}
