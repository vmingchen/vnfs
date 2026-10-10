//! Builders shared by all application clients.
use super::{OpenFlags, OpenOp, Result, Vfsi, VfsiExt};
use std::{fmt, path::Path};

/// `std::fs::OpenOptions`-style builder for any [`Vfsi`] client.
pub struct OpenOptions<'a, C: Vfsi + ?Sized> {
    client: &'a C,
    flags: OpenFlags,
    mode: u32,
}

impl<C: Vfsi + ?Sized> Clone for OpenOptions<'_, C> {
    fn clone(&self) -> Self {
        Self {
            client: self.client,
            flags: self.flags,
            mode: self.mode,
        }
    }
}

impl<C: Vfsi + ?Sized> fmt::Debug for OpenOptions<'_, C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenOptions")
            .field("flags", &self.flags)
            .field("mode", &format_args!("{:#o}", self.mode))
            .finish()
    }
}

impl<'a, C: Vfsi + ?Sized> OpenOptions<'a, C> {
    pub(super) fn new(client: &'a C) -> Self {
        Self {
            client,
            flags: OpenFlags::empty(),
            mode: 0o666,
        }
    }

    pub fn read(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::READ, enabled);
        self
    }

    pub fn write(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::WRITE, enabled);
        self
    }

    pub fn append(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::APPEND, enabled);
        self
    }

    pub fn truncate(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::TRUNCATE, enabled);
        self
    }

    pub fn create(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::CREATE, enabled);
        self
    }

    pub fn create_new(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::CREATE_NEW, enabled);
        self
    }

    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }

    pub fn open(&self, path: impl AsRef<Path>) -> Result<C::File> {
        self.client
            .open_with(OpenOp::new(path.as_ref(), self.flags).mode(self.mode))
    }
}

impl<C: Vfsi + ?Sized> OpenOptions<'_, C> {
    pub fn vopen<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<C::File>> {
        let requests: Vec<OpenOp> = paths
            .iter()
            .map(|path| OpenOp::new(path.as_ref(), self.flags).mode(self.mode))
            .collect();
        self.client.vopen(&requests)
    }
}
