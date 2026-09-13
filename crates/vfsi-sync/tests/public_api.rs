//! Package-boundary checks for the synchronous VFSI interface facets.

use std::path::Path;

use vfsi_sync::{sfsi, vfsi};

fn accepts_scalar(_: &mut dyn sfsi::VecFs) {}
fn accepts_vector(_: &mut dyn vfsi::VecFs) {}
fn accepts_native_scalar(_: &mut dyn sfsi::FileSystem) {}
fn accepts_native_vector(_: &mut dyn vfsi::VectorFileSystem) {}

#[test]
fn scalar_and_vector_facets_share_the_object_safe_contract() {
    fn bridge(fs: &mut dyn vfsi_sync::VecFs) {
        accepts_scalar(fs);
        accepts_vector(fs);
    }

    let _: fn(&mut dyn vfsi_sync::VecFs) = bridge;
}

#[test]
fn native_scalar_and_vector_contracts_are_independently_object_safe() {
    let _: fn(&mut dyn sfsi::FileSystem) = accepts_native_scalar;
    let _: fn(&mut dyn vfsi::VectorFileSystem) = accepts_native_vector;
}

#[test]
fn facets_reexport_the_same_public_types() {
    let scalar = sfsi::VfFile::from_os_path(Path::new("/item"));
    let vector: vfsi::VfFile = scalar;
    assert_eq!(vector.path(), Some(Path::new("/item")));

    let offset = sfsi::VfOffset::At(17);
    assert!(matches!(offset, vfsi::VfOffset::At(17)));
}
