#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unknown_lints)]
#![allow(clippy::missing_safety_doc)]
#![allow(unnecessary_transmutes)]
#![allow(improper_ctypes)]
#![allow(improper_ctypes_definitions)]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(clippy::ptr_offset_with_cast)]
// bindgen 0.73 emits arithmetic div_ceil equivalents in generated bitfield helpers.
#![allow(clippy::manual_div_ceil)]
// bindgen spells C size_t-compatible parameters as c_ulong on this target;
// Rust 1.98's new runtime-symbol lint cannot see that ABI equivalence.
#![allow(suspicious_runtime_symbol_definitions)]

// Keep the native dependency's static archive and linkage metadata attached
// even when a consumer uses only our C codec wrappers.
use libntirpc_sys as _;

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
