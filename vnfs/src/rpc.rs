//! Minimal synchronous NFSv4 RPC layer over libntirpc's CLIENT / clnt_req
//! machinery, mirroring the call pattern used by Ganesha's nfs_rpc_callback.c.

use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

use libntirpc_sys::*;

use crate::error::{RpcError, RpcResult};

pub const NFS4_PROGRAM: rpcprog_t = 100003;
pub const NFS_V4: rpcvers_t = 4;
pub const NFSPROC4_COMPOUND: rpcproc_t = 1;

static SVC_INITED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn svc_req_alloc(xprt: *mut SVCXPRT, xdrs: *mut XDR) -> *mut svc_req {
    let req = libc::calloc(1, std::mem::size_of::<svc_req>()) as *mut svc_req;
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
pub fn svc_init_once() {
    if SVC_INITED.load(Ordering::SeqCst) {
        return;
    }
    unsafe {
        let mut params: svc_init_params = std::mem::zeroed();
        params.flags = (SVC_INIT_EPOLL | SVC_INIT_NOREG_XPRTS) as u_long;
        params.max_events = 16;
        params.alloc_cb = Some(svc_req_alloc);
        params.free_cb = Some(svc_req_free);
        svc_init(&mut params);
        SVC_INITED.store(true, Ordering::SeqCst);
    }
}

/// A connected RPC client on a single TCP transport.
pub struct RpcClient {
    clnt: *mut CLIENT,
    auth: *mut AUTH,
}

unsafe impl Send for RpcClient {}

impl RpcClient {
    /// Connect to `host` for the NFSv4 program using AUTH_SYS (uid 0).
    pub fn connect(host: &str) -> RpcResult<RpcClient> {
        svc_init_once();

        let host = std::ffi::CString::new(host).map_err(|e| RpcError::transport(e.to_string()))?;
        let nettype = std::ffi::CString::new("tcp").unwrap();
        let timeout = timeval {
            tv_sec: 10,
            tv_usec: 0,
        };

        let clnt = unsafe {
            clnt_ncreate_timed(
                host.as_ptr(),
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

        let auth = unsafe { authunix_ncreate_default() };
        Ok(RpcClient { clnt, auth })
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

            let timeout = timespec {
                tv_sec: 5,
                tv_nsec: 0,
            };
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

impl Drop for RpcClient {
    fn drop(&mut self) {
        unsafe {
            let ops = (*(*self.clnt).cl_ops).cl_destroy;
            if let Some(d) = ops {
                d(self.clnt);
            }
        }
    }
}
