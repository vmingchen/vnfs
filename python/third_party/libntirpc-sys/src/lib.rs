#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]
// The generated bindings (bindings.rs) contain variadic-argument helpers and
// bitfield accessors that trip these lints; they are never called from this
// crate and are safe to ignore.
#![allow(unnecessary_transmutes)]
#![allow(improper_ctypes)]
#![allow(improper_ctypes_definitions)]
// Edition 2024 makes unsafe_op_in_unsafe_fn deny-by-default, which the
// bindgen-generated bitfield helpers trip on.
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(clippy::ptr_offset_with_cast)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
pub type rpcblist = rp__list;

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_basic() {
        assert!(unsafe {
            xdr_void(
                std::ptr::null_mut::<rpc_xdr>(),
                std::ptr::null_mut::<std::os::raw::c_void>(),
            )
        });
    }
}
