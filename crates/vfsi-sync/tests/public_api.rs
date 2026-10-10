//! Package-boundary checks for the two native backend contracts.

use vfsi_sync::backend::{HandleBackend, VectorBackend};

fn accepts_handle(_: &mut dyn HandleBackend) {}
fn accepts_vector(_: &mut dyn VectorBackend) {}
fn accepts_backend_as_handle_contract(backend: &mut dyn VectorBackend) {
    accepts_handle(backend);
}

#[test]
fn backend_contracts_are_object_safe_and_vector_includes_handles() {
    let _: fn(&mut dyn HandleBackend) = accepts_handle;
    let _: fn(&mut dyn VectorBackend) = accepts_vector;
    let _: fn(&mut dyn VectorBackend) = accepts_backend_as_handle_contract;
}
