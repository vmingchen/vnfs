use std::path::Path;

/// One directory creation with explicit Unix permission bits.
/// Construction performs no I/O and can borrow the path without allocating.
/// Parents must exist; submit independent siblings together with [`super::Vfsi::vmkdir`].
#[derive(Clone, Copy, Debug)]
pub struct MkDirOp<P> {
    path: P,
    mode: u32,
}

impl<P: AsRef<Path>> MkDirOp<P> {
    /// Prepare a directory creation. The backend applies these permission bits
    /// rather than relying on the process umask.
    pub fn new(path: P, mode: u32) -> Self {
        Self { path, mode }
    }

    /// Directory path in the filesystem's namespace.
    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    /// Requested Unix permission bits.
    pub fn mode(&self) -> u32 {
        self.mode
    }
}
