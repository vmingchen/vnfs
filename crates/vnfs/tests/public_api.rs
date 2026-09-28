//! Compile-time coverage for the application-facing NFS API and the explicit
//! low-level backend namespace. No live server is needed.

use vnfs::{Nfs, NfsAuthentication, NfsBuilder, OpenFlags, OpenRequest, ReadAllOptions};

#[test]
fn application_surface_is_small_and_typed() {
    let _: NfsBuilder = Nfs::builder("server.example.com")
        .root("/export")
        .auth(NfsAuthentication::AuthSys);
    let _ = OpenRequest::new("/file", OpenFlags::READ);
    assert_eq!(ReadAllOptions::new().total_byte_limit(), 16 * 1024 * 1024);
    let _: Option<vnfs::NfsClient> = None;
    let _: Option<vnfs::NfsFile> = None;
    let _: Option<vnfs::Result<()>> = None;
}

#[test]
fn application_traversal_and_mutation_do_not_require_backend_imports() {
    fn app(client: &vnfs::NfsClient) -> vnfs::Result<()> {
        let fields =
            vnfs::MetadataFields::MODE | vnfs::MetadataFields::BLOCKS | vnfs::MetadataFields::NLINK;
        let listings =
            client.read_dirs_with_options(&["/a", "/b"], fields, vnfs::ReadDirOptions::new())?;
        for directory in listings {
            for entry in directory.entries {
                let _ = (entry.path(), entry.metadata().blocks());
            }
        }
        let _ = client.symlink_metadata_with_fields("/a/link", fields)?;
        let _ = client.walk_with_options("/a", fields, vnfs::WalkOptions::new())?;
        client.copy_files(&[("/a/source", "/a/copy")])?;
        client.remove_paths(&["/a/copy"], false)
    }
    let _ = app as fn(&vnfs::NfsClient) -> vnfs::Result<()>;
}

#[test]
fn low_level_types_are_under_backend() {
    use vnfs::backend::{ReadOp, VecFs, VfFile, VfOffset};

    let _ = ReadOp::new(VfFile::from_path("/file"), VfOffset::At(0), 1);
    fn accepts_backend<T: VecFs + ?Sized>(_: &mut T) {}
    let _ = accepts_backend::<dyn VecFs>;
}
