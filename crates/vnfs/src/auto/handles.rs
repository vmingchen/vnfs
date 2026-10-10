//! Handles pinned to their selected route and owning client.

use super::*;

pub(super) enum AutoDirInner {
    Mounted(vfsi_sync::FsDir<DummyVecFs>),
    Nfs(vfsi_sync::FsDir<vfsi_nfs::NfsVecFs>),
}

/// Owned handle-rooted directory. Path-only backends fail at open rather than
/// weakening the handle-rooted safety contract.
pub struct AutoDir {
    pub(super) path: PathBuf,
    pub(super) route: Route,
    pub(super) inner: AutoDirInner,
    pub(super) owner: Arc<()>,
}

impl std::fmt::Debug for AutoDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoDir")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl AutoDir {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn is_closed(&self) -> bool {
        match &self.inner {
            AutoDirInner::Mounted(dir) => dir.is_closed(),
            AutoDirInner::Nfs(dir) => dir.is_closed(),
        }
    }
    pub fn try_close(&mut self) -> VfResult<()> {
        match &mut self.inner {
            AutoDirInner::Mounted(dir) => dir.try_close(),
            AutoDirInner::Nfs(dir) => dir.try_close(),
        }
    }
    pub fn close(mut self) -> VfResult<()> {
        self.try_close()
    }
}

pub(super) enum AutoFileInner {
    Mounted(FsFile<DummyVecFs>),
    Nfs(NfsFile),
}

/// Open file pinned to the backend chosen when it was opened.
pub struct AutoFile {
    pub(super) path: PathBuf,
    pub(super) route: Route,
    pub(super) inner: AutoFileInner,
    pub(super) owner: Arc<()>,
}

impl std::fmt::Debug for AutoFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AutoFile")
            .field("path", &self.path)
            .field("route", &self.route.public())
            .finish_non_exhaustive()
    }
}

impl AutoFile {
    pub fn is_closed(&self) -> bool {
        match &self.inner {
            AutoFileInner::Mounted(file) => file.is_closed(),
            AutoFileInner::Nfs(file) => file.is_closed(),
        }
    }
    pub(super) fn new(path: PathBuf, route: Route, inner: AutoFileInner, owner: &Arc<()>) -> Self {
        Self {
            path,
            route,
            inner,
            owner: Arc::clone(owner),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn check_credentials(&self) -> VfResult<()> {
        if let Route::Nfs(connection) = &self.route
            && AuthSysIdentity::current().as_ref() != Some(&connection.credentials)
        {
            return Err(
                VfError::client(0, libc::EACCES as u32).with_context("auto_auth", &self.path)
            );
        }
        Ok(())
    }
    pub fn route(&self) -> AutoRoute {
        self.route.public()
    }

    pub fn try_close(&mut self) -> VfResult<()> {
        match &mut self.inner {
            AutoFileInner::Mounted(file) => file.try_close(),
            AutoFileInner::Nfs(file) => file.try_close(),
        }
    }
    pub fn close(self) -> VfResult<()> {
        match self.inner {
            AutoFileInner::Mounted(file) => file.close(),
            AutoFileInner::Nfs(file) => file.close(),
        }
    }
}

pub(super) struct AutoRead<'a> {
    pub(super) file: &'a AutoFile,
    pub(super) offset: u64,
    pub(super) length: usize,
}
pub(super) struct AutoReadInto<'a> {
    pub(super) file: &'a AutoFile,
    pub(super) offset: u64,
    pub(super) buffer: &'a mut [u8],
}
