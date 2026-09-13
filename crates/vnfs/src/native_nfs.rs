//! Application-facing NFS constructor and concrete client aliases.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::{
    FsClient, FsFile, NfsAuthentication, NfsClientBuilder, NfsObserver, NfsRecoveryPolicy,
    NfsVecFs, VfResult,
};

pub type NfsClient = FsClient<NfsVecFs>;
pub type NfsFile = FsFile<NfsVecFs>;

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

    pub fn authentication(mut self, authentication: NfsAuthentication) -> Self {
        self.inner = self.inner.authentication(authentication);
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

    pub fn require_secure_authentication(mut self, required: bool) -> Self {
        self.inner = self.inner.require_secure_authentication(required);
        self
    }

    pub fn connect(self) -> VfResult<NfsClient> {
        self.inner.connect().map(FsClient::new)
    }

    pub fn connect_backend(self) -> VfResult<NfsVecFs> {
        self.inner.connect()
    }
}
