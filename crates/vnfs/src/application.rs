//! Concrete client implementations of the portable VFSI contracts.
use crate::{
    Attrs, DirectoryListing, FileHandle, OpenOp, ResourceLimits, Result, Vfsi, WriteResult,
};
use std::path::Path;
macro_rules! client_methods {
    ($client:ty, $receiver:path) => {
        client_methods!($client, $receiver, <$client>::vread_impl, $receiver);
    };
    ($client:ty, $receiver:path, $read:expr, $read_receiver:path) => {
        client_methods!(
            $client,
            $receiver,
            $read,
            $read_receiver,
            <$client>::write_partial_native,
            <$client>::write_complete,
            <$client>::vgetattrs_impl,
            $receiver
        );
    };
    ($client:ty, $receiver:path, $read:expr, $read_receiver:path,
     $partial:expr, $complete:expr, $attrs:expr, $write_receiver:path) => {
        vfsi_sync::__vfsi_client_methods!(
            $client,
            $receiver,
            $read,
            $read_receiver,
            $partial,
            $complete,
            $attrs,
            $write_receiver,
            <$client>::directory_page_batch_size,
            <$client>::open_native,
            <$client>::stream_native;
            vrename_impl, vsymlink_impl, vreadlink_impl, vhardlink_impl,
            vstatfs_impl, vsetattrs_impl, limits_impl, vopen_impl,
            vclose_impl, vmkdir_impl, vcopy_impl, capabilities_impl
        );
    };
}

#[cfg(feature = "nfs")]
impl FileHandle for crate::NfsFile {
    vfsi_sync::__vfsi_file_methods!(crate::NfsFile);
}
#[cfg(feature = "nfs")]
impl Vfsi for crate::NfsClient {
    type File = crate::NfsFile;
    type Dir = crate::NfsDir;
    client_methods!(crate::NfsClient, std::convert::identity);
}

#[cfg(all(feature = "auto", target_os = "linux"))]
mod routed {
    use super::*;
    impl FileHandle for crate::AutoFile {
        vfsi_sync::__vfsi_file_methods!(crate::AutoFile);
    }
    impl Vfsi for crate::Auto {
        type File = crate::AutoFile;
        type Dir = crate::AutoDir;
        client_methods!(crate::Auto, std::convert::identity);
    }

    impl Vfsi for crate::Mounted {
        type File = crate::MountedFile;
        type Dir = crate::MountedDir;
        client_methods!(crate::Mounted, std::convert::identity);
    }

    impl FileHandle for crate::MountedFile {
        vfsi_sync::__vfsi_file_methods!(crate::MountedFile);
    }
}

#[cfg(all(test, feature = "auto", target_os = "linux"))]
mod extension_tests;

#[cfg(feature = "nfs")]
impl crate::DirHandle for crate::NfsDir {
    vfsi_sync::__vfsi_file_methods!(crate::NfsDir);
}
#[cfg(all(feature = "auto", target_os = "linux"))]
impl crate::DirHandle for crate::MountedDir {
    vfsi_sync::__vfsi_file_methods!(crate::MountedDir);
}
#[cfg(all(feature = "auto", target_os = "linux"))]
impl crate::DirHandle for crate::AutoDir {
    vfsi_sync::__vfsi_file_methods!(crate::AutoDir);
}
