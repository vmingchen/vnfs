#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unknown_lints)]
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
#![allow(clippy::manual_div_ceil)]
#![allow(suspicious_runtime_symbol_definitions)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
pub type rpcblist = rp__list;

unsafe extern "C" {
    /// Release an AUTH reference using libntirpc's `auth_destroy` macro.
    pub fn vfsi_libntirpc_auth_destroy(auth: *mut AUTH);
    /// Set a server transport's process callback without exposing bindgen's
    /// version-dependent representation of the anonymous dispatch union.
    pub fn vfsi_libntirpc_set_process_cb(xprt: *mut SVCXPRT, callback: svc_req_fun_t);
    #[cfg(feature = "rpcsec-gss")]
    pub fn vfsi_libntirpc_authgss_ncreate_default(
        client: *mut CLIENT,
        service: *mut ::std::os::raw::c_char,
        security: *mut ::std::os::raw::c_void,
    ) -> *mut AUTH;
    #[cfg(feature = "rpcsec-gss")]
    pub fn vfsi_libntirpc_install_reply_verifier_fix(client: *mut CLIENT) -> bool;
    #[cfg(feature = "rpcsec-gss")]
    pub fn vfsi_libntirpc_uninstall_reply_verifier_fix(client: *mut CLIENT);
}

/// ABI declarations for libntirpc's optional RPCSEC_GSS client support.
///
/// These definitions intentionally live outside the generated base bindings:
/// including `<rpc/auth_gss.h>` makes bindgen's output depend on the host's
/// complete GSS implementation.  The small public ABI below has been stable
/// across supported libntirpc releases.
#[cfg(feature = "rpcsec-gss")]
pub mod rpcsec_gss {
    use std::os::raw::{c_char, c_int, c_uint, c_void};

    use super::{AUTH, CLIENT};

    pub const RPCSEC_GSS_SVC_NONE: c_int = 1;
    pub const RPCSEC_GSS_SVC_INTEGRITY: c_int = 2;

    /// libntirpc's `struct rpc_gss_sec`.
    #[repr(C)]
    #[derive(Debug, Copy, Clone)]
    pub struct RpcGssSec {
        /// `GSS_C_NO_OID` selects the implementation's default mechanism.
        pub mech: *mut c_void,
        /// `GSS_C_QOP_DEFAULT` is zero.
        pub qop: c_uint,
        pub svc: c_int,
        /// `GSS_C_NO_CREDENTIAL` uses the process's default credential cache.
        pub cred: *mut c_void,
        pub req_flags: c_uint,
    }

    unsafe extern "C" {
        pub fn authgss_ncreate_default(
            client: *mut CLIENT,
            service: *mut c_char,
            security: *mut RpcGssSec,
        ) -> *mut AUTH;
    }
}

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
