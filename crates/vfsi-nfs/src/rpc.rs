//! Minimal synchronous NFSv4 RPC layer over libntirpc's CLIENT / clnt_req
//! machinery, mirroring the call pattern used by Ganesha's nfs_rpc_callback.c.

use std::os::raw::c_void;
#[cfg(not(libntirpc_legacy_free_cb))]
use std::os::raw::{c_char, c_int};
use std::sync::OnceLock;
use std::time::Duration;

use libntirpc_sys::*;

use crate::error::{RpcError, RpcResult};

pub const NFS4_PROGRAM: rpcprog_t = 100003;
pub const NFS_V4: rpcvers_t = 4;
pub const NFSPROC4_COMPOUND: rpcproc_t = 1;

static SVC_INIT: OnceLock<Result<(), String>> = OnceLock::new();

/// Authentication used for the NFS RPC connection.
///
/// AUTH_SYS remains the default for compatibility. RPCSEC_GSS is available
/// only when the `rpcsec-gss` Cargo feature is enabled and obtains Kerberos
/// credentials from the process's default GSS credential cache; passwords
/// and other secrets are never accepted by this API.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum NfsAuthentication {
    /// Traditional host-trusted AUTH_SYS credentials for the current process.
    #[default]
    AuthSys,
    /// Kerberos-backed RPCSEC_GSS authentication.
    #[cfg(feature = "rpcsec-gss")]
    RpcsecGss {
        /// GSS host-based service name. `None` uses `nfs@<host>`.
        service_principal: Option<String>,
        /// Protection applied to NFS RPC payloads.
        protection: RpcsecGssProtection,
    },
}

/// RPCSEC_GSS data protection level.
#[cfg(feature = "rpcsec-gss")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum RpcsecGssProtection {
    /// Authenticate requests (`sec=krb5`) without protecting RPC payloads.
    Authentication,
    /// Authenticate requests and integrity-protect payloads (`sec=krb5i`).
    #[default]
    Integrity,
}

#[cfg(feature = "rpcsec-gss")]
impl RpcsecGssProtection {
    fn service_code(self) -> std::os::raw::c_int {
        use libntirpc_sys::rpcsec_gss::{RPCSEC_GSS_SVC_INTEGRITY, RPCSEC_GSS_SVC_NONE};
        match self {
            Self::Authentication => RPCSEC_GSS_SVC_NONE,
            Self::Integrity => RPCSEC_GSS_SVC_INTEGRITY,
        }
    }

    fn context_flags(self) -> std::os::raw::c_uint {
        // Match NFS-Ganesha's libntirpc client setup. RPCSEC_GSS has its own
        // sequence window, so RFC 2203 says not to request GSS replay/sequence
        // flags. Kerberos contexts provide integrity/confidentiality support;
        // the RPC service selection below decides whether payloads use it.
        const GSS_C_MUTUAL_FLAG: std::os::raw::c_uint = 2;
        const GSS_C_INTEG_FLAG: std::os::raw::c_uint = 32;
        match self {
            Self::Authentication => 0,
            Self::Integrity => GSS_C_MUTUAL_FLAG | GSS_C_INTEG_FLAG,
        }
    }
}

unsafe extern "C" fn svc_req_alloc(xprt: *mut SVCXPRT, xdrs: *mut XDR) -> *mut svc_req {
    let req = unsafe { libc::calloc(1, std::mem::size_of::<svc_req>()) } as *mut svc_req;
    if req.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        (*req).rq_xprt = xprt;
        (*req).rq_xdrs = xdrs;
    }
    req
}

unsafe extern "C" fn svc_req_free(req: *mut svc_req, _stat: xprt_stat) {
    unsafe { libc::free(req as *mut c_void) }
}

/// Initialize the ntirpc service machinery once.  Client replies are processed
/// by the same epoll/request machinery as server calls, so the request
/// alloc/free callbacks are required on the client too.
pub fn svc_init_once() -> RpcResult<()> {
    let result = SVC_INIT.get_or_init(|| unsafe {
        let mut params: svc_init_params = std::mem::zeroed();
        params.flags = (SVC_INIT_EPOLL | SVC_INIT_NOREG_XPRTS) as u_long;
        params.max_events = 16;
        params.alloc_cb = Some(svc_req_alloc);
        params.free_cb = Some(svc_req_free);
        if svc_init(&mut params) {
            Ok(())
        } else {
            Err("libntirpc svc_init failed".to_string())
        }
    });
    match result {
        Ok(()) => Ok(()),
        Err(message) => Err(RpcError::transport(message.clone())),
    }
}

/// A connected RPC client on a single TCP transport.
pub struct RpcClient {
    clnt: *mut CLIENT,
    auth: *mut AUTH,
    request_timeout: timespec,
    #[cfg(feature = "rpcsec-gss")]
    reply_verifier_fix_installed: bool,
}

unsafe impl Send for RpcClient {}

impl RpcClient {
    /// Connect to `host` for the NFSv4 program using AUTH_SYS.
    pub fn connect(host: &str) -> RpcResult<RpcClient> {
        Self::connect_with_timeouts(host, Duration::from_secs(10), Duration::from_secs(5))
    }

    /// Connect with explicit bounds for connection setup and each RPC call.
    pub fn connect_with_timeouts(
        host: &str,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> RpcResult<RpcClient> {
        Self::connect_with_authentication(
            host,
            connect_timeout,
            request_timeout,
            &NfsAuthentication::AuthSys,
        )
    }

    /// Connect with explicit timeouts and authentication.
    pub fn connect_with_authentication(
        host: &str,
        connect_timeout: Duration,
        request_timeout: Duration,
        authentication: &NfsAuthentication,
    ) -> RpcResult<RpcClient> {
        svc_init_once()?;

        let host_c =
            std::ffi::CString::new(host).map_err(|e| RpcError::transport(e.to_string()))?;
        let nettype = std::ffi::CString::new("tcp").unwrap();
        let mut timeout = timeval {
            tv_sec: connect_timeout.as_secs().min(i64::MAX as u64) as _,
            tv_usec: connect_timeout.subsec_micros() as _,
        };
        // `timeval` has microsecond precision. Preserve a positive caller
        // timeout instead of rounding sub-microsecond durations to "no wait".
        if !connect_timeout.is_zero() && timeout.tv_sec == 0 && timeout.tv_usec == 0 {
            timeout.tv_usec = 1;
        }

        let clnt = unsafe {
            clnt_ncreate_timed(
                host_c.as_ptr(),
                NFS4_PROGRAM,
                NFS_V4,
                nettype.as_ptr(),
                &timeout,
            )
        };
        if clnt.is_null() {
            return Err(RpcError::transport("clnt_ncreate_timed returned NULL"));
        }
        unsafe {
            if (*clnt).cl_error.re_status != clnt_stat_RPC_SUCCESS {
                let e = format!(
                    "failed to create client: rpc_err {}",
                    (*clnt).cl_error.re_status
                );
                let ops = (*(*clnt).cl_ops).cl_destroy;
                if let Some(d) = ops {
                    d(clnt);
                }
                return Err(RpcError::transport(e));
            }
        }

        let auth = match authentication {
            NfsAuthentication::AuthSys => unsafe { authunix_ncreate_default() },
            #[cfg(feature = "rpcsec-gss")]
            NfsAuthentication::RpcsecGss {
                service_principal,
                protection,
            } => {
                use libntirpc_sys::rpcsec_gss::RpcGssSec;

                if !unsafe { vfsi_libntirpc_install_reply_verifier_fix(clnt) } {
                    unsafe { destroy_client(clnt) };
                    return Err(RpcError::transport(
                        "failed to install libntirpc RPCSEC_GSS reply-verifier compatibility fix",
                    ));
                }

                let principal = service_principal
                    .clone()
                    .unwrap_or_else(|| format!("nfs@{host}"));
                let principal = std::ffi::CString::new(principal).map_err(|error| {
                    unsafe {
                        vfsi_libntirpc_uninstall_reply_verifier_fix(clnt);
                        destroy_client(clnt);
                    }
                    RpcError::transport(format!("invalid RPCSEC_GSS service principal: {error}"))
                })?;
                let mut security = RpcGssSec {
                    mech: std::ptr::null_mut(),
                    qop: 0,
                    svc: protection.service_code(),
                    cred: std::ptr::null_mut(),
                    req_flags: protection.context_flags(),
                };
                unsafe {
                    vfsi_libntirpc_authgss_ncreate_default(
                        clnt,
                        principal.as_ptr().cast_mut(),
                        (&mut security as *mut RpcGssSec).cast::<c_void>(),
                    )
                }
            }
        };
        if auth.is_null() {
            unsafe {
                #[cfg(feature = "rpcsec-gss")]
                if matches!(authentication, NfsAuthentication::RpcsecGss { .. }) {
                    vfsi_libntirpc_uninstall_reply_verifier_fix(clnt);
                }
                destroy_client(clnt);
            }
            return Err(RpcError::transport(match authentication {
                NfsAuthentication::AuthSys => "failed to create AUTH_SYS credentials",
                #[cfg(feature = "rpcsec-gss")]
                NfsAuthentication::RpcsecGss { .. } => {
                    "RPCSEC_GSS negotiation failed; obtain a valid Kerberos ticket and verify the NFS service principal"
                }
            }));
        }
        if unsafe { (*auth).ah_error.re_status } != clnt_stat_RPC_SUCCESS {
            let status = unsafe { (*auth).ah_error.re_status };
            unsafe {
                destroy_auth(auth);
                #[cfg(feature = "rpcsec-gss")]
                if matches!(authentication, NfsAuthentication::RpcsecGss { .. }) {
                    vfsi_libntirpc_uninstall_reply_verifier_fix(clnt);
                }
                destroy_client(clnt);
            }
            return Err(RpcError::transport(format!(
                "RPC authentication setup failed: rpc_err {status}"
            )));
        }
        Ok(RpcClient {
            clnt,
            auth,
            request_timeout: timespec {
                tv_sec: request_timeout.as_secs().min(i64::MAX as u64) as _,
                tv_nsec: request_timeout.subsec_nanos() as _,
            },
            #[cfg(feature = "rpcsec-gss")]
            reply_verifier_fix_installed: matches!(
                authentication,
                NfsAuthentication::RpcsecGss { .. }
            ),
        })
    }

    /// Synchronous RPC call: encode `args` with `xargs`, decode the reply with
    /// `xres` into `res`.  `res` must be freed by the caller using the
    /// xdr_free-null-stream idiom.
    pub fn call(
        &self,
        proc_: rpcproc_t,
        xargs: xdrproc_t,
        args: *mut c_void,
        xres: xdrproc_t,
        res: *mut c_void,
    ) -> RpcResult<()> {
        unsafe {
            let mut req = Box::new(std::mem::zeroed::<clnt_req>());
            let reqp: *mut clnt_req = &mut *req;

            (*reqp).cc_clnt = self.clnt;
            (*reqp).cc_auth = self.auth;
            (*reqp).cc_proc = proc_;
            (*reqp).cc_call = xdrpair {
                proc_: xargs,
                where_: args,
            };
            (*reqp).cc_reply = xdrpair {
                proc_: xres,
                where_: res,
            };
            (*reqp).cc_verf = _null_auth;
            (*reqp).cc_free_cb = Some(req_free_cb);
            (*reqp).cc_size = std::mem::size_of::<clnt_req>();
            (*reqp).cc_refcnt = 1;

            libc::pthread_mutex_init(
                &mut (*reqp).cc_we.mtx as *mut _ as *mut libc::pthread_mutex_t,
                std::ptr::null(),
            );
            libc::pthread_cond_init(
                &mut (*reqp).cc_we.cv as *mut _ as *mut libc::pthread_cond_t,
                std::ptr::null(),
            );
            libc::pthread_mutex_lock(
                &mut (*reqp).cc_we.mtx as *mut _ as *mut libc::pthread_mutex_t,
            );

            let timeout = self.request_timeout;
            let stat = clnt_req_setup(reqp, timeout);
            if stat != clnt_stat_RPC_SUCCESS {
                clnt_req_release(reqp);
                std::mem::forget(req);
                return Err(RpcError::transport(format!(
                    "clnt_req_setup failed: {}",
                    stat
                )));
            }
            (*reqp).cc_refreshes = 1;
            let status = clnt_req_wait_reply(reqp);
            let err = (*reqp).cc_error;
            clnt_req_release(reqp);
            // clnt_req_release runs our free callback, which frees the Box;
            // prevent the compiler from dropping it a second time.
            std::mem::forget(req);
            if status != clnt_stat_RPC_SUCCESS {
                return Err(RpcError::transport(format!(
                    "RPC call failed: stat={}, rpc_err.status={}",
                    status, err.re_status
                )));
            }
            Ok(())
        }
    }
}

unsafe fn destroy_client(client: *mut CLIENT) {
    unsafe {
        if !client.is_null()
            && !(*client).cl_ops.is_null()
            && let Some(destroy) = (*(*client).cl_ops).cl_destroy
        {
            destroy(client);
        }
    }
}

unsafe fn destroy_auth(auth: *mut AUTH) {
    unsafe {
        if !auth.is_null() {
            vfsi_libntirpc_auth_destroy(auth);
        }
    }
}

#[cfg(not(libntirpc_legacy_free_cb))]
unsafe extern "C" fn req_free_cb(
    cc: *mut clnt_req,
    _size: usize,
    _file: *const c_char,
    _line: c_int,
    _func: *const c_char,
) {
    unsafe {
        drop(Box::from_raw(cc));
    }
}

#[cfg(libntirpc_legacy_free_cb)]
unsafe extern "C" fn req_free_cb(cc: *mut clnt_req, _size: usize) {
    unsafe {
        drop(Box::from_raw(cc));
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        unsafe {
            // RPCSEC_GSS destruction sends its context-destroy request over
            // the client transport, so authentication must be released first.
            destroy_auth(self.auth);
            #[cfg(feature = "rpcsec-gss")]
            if self.reply_verifier_fix_installed {
                vfsi_libntirpc_uninstall_reply_verifier_fix(self.clnt);
            }
            destroy_client(self.clnt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authentication_defaults_to_auth_sys() {
        assert_eq!(NfsAuthentication::default(), NfsAuthentication::AuthSys);
    }

    #[cfg(feature = "rpcsec-gss")]
    #[test]
    fn rpcsec_gss_levels_map_to_wire_services_and_required_context_flags() {
        use libntirpc_sys::rpcsec_gss::{RPCSEC_GSS_SVC_INTEGRITY, RPCSEC_GSS_SVC_NONE};

        let auth = RpcsecGssProtection::Authentication;
        let integrity = RpcsecGssProtection::Integrity;
        assert_eq!(auth.service_code(), RPCSEC_GSS_SVC_NONE);
        assert_eq!(integrity.service_code(), RPCSEC_GSS_SVC_INTEGRITY);
        assert_eq!(RpcsecGssProtection::default(), integrity);

        const MUTUAL: u32 = 2;
        const REPLAY: u32 = 4;
        const SEQUENCE: u32 = 8;
        const INTEGRITY: u32 = 32;
        assert_eq!(auth.context_flags(), 0);
        assert_eq!(integrity.context_flags(), MUTUAL | INTEGRITY);
        for level in [auth, integrity] {
            assert_eq!(level.context_flags() & (REPLAY | SEQUENCE), 0);
        }
    }

    #[cfg(feature = "rpcsec-gss")]
    #[test]
    fn rpc_gss_security_triple_matches_the_supported_c_abi() {
        use libntirpc_sys::rpcsec_gss::RpcGssSec;

        // The supported production target is 64-bit Linux. This catches an
        // accidental field type/order change in our deliberately small ABI
        // declaration before it reaches libntirpc.
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<RpcGssSec>(), 32);
        assert_eq!(
            std::mem::align_of::<RpcGssSec>(),
            std::mem::align_of::<usize>()
        );
    }
}
