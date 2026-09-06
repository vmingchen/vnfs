#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]
#![allow(unnecessary_transmutes)]
#![allow(improper_ctypes)]
#![allow(improper_ctypes_definitions)]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(clippy::ptr_offset_with_cast)]
// bindgen spells C size_t-compatible parameters as c_ulong on this target;
// Rust 1.98's new runtime-symbol lint cannot see that ABI equivalence.
#![allow(suspicious_runtime_symbol_definitions)]

// The codec wrappers in wrapper.c (compiled into this crate's rlib)
// reference libntirpc.so symbols directly, so any binary that uses this
// crate must also link the dynamic library. `#[link]` ensures rustc
// re-emits -lntirpc at final link time whenever this rlib is pulled in.
#[link(name = "ntirpc", kind = "dylib")]
unsafe extern "C" {}

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
