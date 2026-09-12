//! A minimal RPC client that greets a hello server.
//!
//! Requires the `hello_server` example to be running and an `rpcbind`
//! daemon to be up so the client can look up the server's port. Run it as:
//!
//! ```sh
//! cargo run --example hello_client -- [name] [host]
//! ```

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

use libntirpc_sys::*;

const HELLO_PROG: rpcprog_t = 0x3000_0001;
const HELLO_VERS: rpcvers_t = 1;
const HELLO_PROC: rpcproc_t = 1;

/// bindgen emits `xdr_wrapstring` with the concrete argument type
/// `*mut *mut c_char`, so wrap it with the generic `xdrproc_t` signature.
unsafe extern "C" fn wrap_string(xdrs: *mut XDR, arg: *mut c_void) -> bool {
    unsafe { xdr_wrapstring(xdrs, arg as *mut *mut c_char) }
}

/// Client replies are processed by the same request machinery as server
/// calls, so the request alloc/free callbacks are required on the client too.
unsafe extern "C" fn svc_req_alloc(xprt: *mut SVCXPRT, xdrs: *mut XDR) -> *mut svc_req {
    unsafe {
        let req = libc::calloc(1, std::mem::size_of::<svc_req>()) as *mut svc_req;
        (*req).rq_xprt = xprt;
        (*req).rq_xdrs = xdrs;
        req
    }
}

unsafe extern "C" fn svc_req_free(req: *mut svc_req, _stat: xprt_stat) {
    unsafe { libc::free(req as *mut c_void) }
}

fn main() {
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "world".to_string());
    let host = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let name_c = CString::new(name).unwrap();
    let host_c = CString::new(host).unwrap();
    let nettype = c"udp";

    unsafe {
        // ntirpc requires svc_init() before any RPC activity, even for
        // clients. SVC_INIT_EPOLL must be set or max_events stays 0 and
        // the event loop that processes replies breaks with EINVAL.
        // SVC_INIT_NOREG_XPRTS is required too: otherwise svc_dg_ncreatef
        // registers the reply socket with the event channel before ntirpc
        // has installed its rendezvous callback, and a fast reply (from
        // rpcbind on localhost) can crash in svc_dg_rendezvous().
        let mut params: svc_init_params = std::mem::zeroed();
        params.flags = (SVC_INIT_EPOLL | SVC_INIT_NOREG_XPRTS) as u_long;
        params.max_events = 16;
        params.alloc_cb = Some(svc_req_alloc);
        params.free_cb = Some(svc_req_free);
        svc_init(&mut params);

        // rpc_call() performs a one-shot synchronous RPC: send a name,
        // receive the greeting.
        let mut name_ptr: *mut c_char = name_c.as_ptr() as *mut c_char;
        let mut result: *mut c_char = std::ptr::null_mut();

        let stat = rpc_call(
            host_c.as_ptr(),
            HELLO_PROG,
            HELLO_VERS,
            HELLO_PROC,
            Some(wrap_string),
            &mut name_ptr as *mut *mut c_char as *const c_void,
            Some(wrap_string),
            &mut result as *mut *mut c_char as *mut c_void,
            nettype.as_ptr(),
        );

        if stat != clnt_stat_RPC_SUCCESS {
            eprintln!("RPC call failed with status {stat}");
            std::process::exit(1);
        }

        let reply = CStr::from_ptr(result);
        println!("{}", reply.to_string_lossy());
        libc::free(result as *mut c_void);
    }
}
