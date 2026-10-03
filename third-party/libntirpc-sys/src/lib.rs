#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unknown_lints)]
#![allow(clippy::missing_safety_doc)]
// The generated bindings (bindings.rs) contain variadic-argument helpers and
// bitfield accessors that trip these lints; they are never called from this
// crate and are safe to ignore.
#![allow(unnecessary_transmutes)]
// Bindgen output trips these. They cannot be scoped to the include! because
// the macros expand at module scope; audit hand-written code in this file
// separately instead.
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

#[cfg(feature = "fuzzing")]
pub mod fuzzing;

unsafe extern "C" {
    /// Release an AUTH reference using libntirpc's `auth_destroy` macro.
    pub fn vfsi_libntirpc_auth_destroy(auth: *mut AUTH);
    /// Set a server transport's process callback without exposing bindgen's
    /// version-dependent representation of the anonymous dispatch union.
    pub fn vfsi_libntirpc_set_process_cb(xprt: *mut SVCXPRT, callback: svc_req_fun_t);
    /// `sizeof(SVCXPRT)` as the C shims see it. Used to assert that the
    /// generated bindings and the shims agree on the transport ABI.
    pub fn vfsi_libntirpc_sizeof_svcxprt() -> usize;
    #[cfg(feature = "rpcsec-gss")]
    pub fn vfsi_libntirpc_authgss_ncreate_default(
        client: *mut CLIENT,
        service: *mut ::std::os::raw::c_char,
        security: *mut ::std::os::raw::c_void,
    ) -> *mut AUTH;
    /// Install the RPCSEC_GSS reply-verifier shim on a client transport.
    ///
    /// libntirpc reads `xp_ops` with plain, unlocked loads and registers the
    /// transport with its worker before this can be called, so the install is
    /// a best-effort pointer swap, not a synchronised race-free operation. It
    /// must only be used before the first call and never on a transport that
    /// is already serving (for example one shared via `clnt_vc_ncreate_svc`).
    /// See `src/auth_helpers.c` for the full rationale.
    #[cfg(feature = "rpcsec-gss")]
    pub fn vfsi_libntirpc_install_reply_verifier_fix(client: *mut CLIENT) -> bool;
    /// Kept for API compatibility but intentionally does not touch the
    /// transport: libntirpc dispatches `xp_ops` without locking, so restoring
    /// or freeing the shim would race an in-flight decode (see
    /// `auth_helpers.c`).
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
    fn memory_xdr_encodes_decodes_and_frees_a_string() {
        unsafe {
            let input = std::ffi::CString::new("abc").unwrap();
            let mut pointer = input.as_ptr() as *mut std::os::raw::c_char;
            // xdrmem_ncreate requires four-byte-aligned backing storage.
            let mut storage = [0_u32; 2];
            let bytes = std::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<u8>(), 8);
            let mut stream: XDR = std::mem::zeroed();
            xdrmem_ncreate(&mut stream, bytes.as_mut_ptr().cast(), 8, xdr_op_XDR_ENCODE);
            assert!(xdr_wrapstring(&mut stream, &mut pointer));
            assert_eq!(bytes, &[0, 0, 0, 3, b'a', b'b', b'c', 0]);
            xdrmem_ncreate(&mut stream, bytes.as_mut_ptr().cast(), 8, xdr_op_XDR_DECODE);
            let mut decoded = std::ptr::null_mut();
            assert!(xdr_wrapstring(&mut stream, &mut decoded));
            assert_eq!(std::ffi::CStr::from_ptr(decoded), input.as_c_str());
            assert!(xdr_wrapstring(&raw mut xdr_free_null_stream, &mut decoded));
            assert!(
                decoded.is_null(),
                "XDR_FREE must release and clear the allocation"
            );
        }
    }

    /// The bindgen invocation and the C shims must compile `SVCXPRT` with the
    /// same feature macros (notably `INET6`), otherwise every field of the
    /// private `rpc_dplx_rec` after the transport is misaligned.
    #[test]
    fn svcxprt_abi_matches_shims() {
        let bindgen = std::mem::size_of::<SVCXPRT>();
        let shims = unsafe { vfsi_libntirpc_sizeof_svcxprt() };
        assert_eq!(
            bindgen, shims,
            "bindgen and the C shims disagree on sizeof(SVCXPRT) \
             ({bindgen} vs {shims}); check the -D flags passed to both",
        );
    }
}
