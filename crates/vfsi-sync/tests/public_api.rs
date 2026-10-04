//! Package-boundary checks for the two native synchronous contracts.

use std::path::Path;

use vfsi_sync::{sfsi, vfsi};

fn accepts_scalar(_: &mut dyn sfsi::FileSystem) {}
fn accepts_vector(_: &mut dyn vfsi::Backend) {}
fn accepts_backend_as_handle_contract(backend: &mut dyn vfsi::Backend) {
    accepts_scalar(backend);
}

#[test]
fn handle_and_backend_contracts_are_object_safe_and_backend_includes_handles() {
    let _: fn(&mut dyn sfsi::FileSystem) = accepts_scalar;
    let _: fn(&mut dyn vfsi_sync::Backend) = accepts_vector;
    let _: fn(&mut dyn vfsi::Backend) = accepts_backend_as_handle_contract;
}

#[test]
fn facets_reexport_the_same_public_types() {
    let scalar = sfsi::VfFile::from_os_path(Path::new("/item"));
    let vector: vfsi::VfFile = scalar;
    assert_eq!(vector.path(), Some(Path::new("/item")));

    let offset = sfsi::VfOffset::At(17);
    assert!(matches!(offset, vfsi::VfOffset::At(17)));
}
