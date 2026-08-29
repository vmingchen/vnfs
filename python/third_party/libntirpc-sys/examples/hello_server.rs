//! A minimal RPC server that greets callers.
//!
//! Requires an `rpcbind` daemon to be running so the server can register
//! its program number. Run it as:
//!
//! ```sh
//! cargo run --example hello_server
//! ```
//!
//! ntirpc does not wire up request handling for you: the server is expected
//! to install a `rendezvous_cb` on the listening transport (which hooks the
//! per-request `process_cb`) just like nfs-ganesha does. See the comments
//! below.

use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::time::Duration;

use libc::{AF_INET, INADDR_ANY, SOCK_DGRAM};
use libntirpc_sys::*;

const HELLO_PROG: rpcprog_t = 0x3000_0001;
const HELLO_VERS: rpcvers_t = 1;

/// bindgen emits `xdr_wrapstring` with the concrete argument type
/// `*mut *mut c_char`, so wrap it with the generic `xdrproc_t` signature.
unsafe extern "C" fn wrap_string(xdrs: *mut XDR, arg: *mut c_void) -> bool {
    unsafe { xdr_wrapstring(xdrs, arg as *mut *mut c_char) }
}

/// ntirpc calls `alloc_cb` to allocate a request for each incoming call.
/// It requires the request to be zeroed and the transport/XDR set.
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

/// The dispatch registered with `svc_reg` is only used for bookkeeping;
/// ntirpc never calls it (requests go to `process_cb` instead).
unsafe extern "C" fn hello_dispatch(_req: *mut svc_req) {}

/// Called by ntirpc for every incoming call (via `process_cb`). The client
/// sends its name as a string and receives "Hello, <name>!" back.
unsafe extern "C" fn hello_process(req: *mut svc_req) -> xprt_stat {
    unsafe {
        // ntirpc does not authenticate requests itself; the server must call
        // svc_auth_authenticate() to set up req->rq_auth. Without it, rq_auth is
        // NULL and svc_sendreply() skips the auth wrap, which is what actually
        // encodes the result body (so the client would get an empty reply).
        let mut no_dispatch = false;
        if svc_auth_authenticate(req, &mut no_dispatch) != auth_stat_AUTH_OK {
            return xprt_stat_XPRT_IDLE;
        }

        let mut name: *mut c_char = std::ptr::null_mut();

        if !xdr_wrapstring((*req).rq_xdrs, &mut name) {
            // Could not decode the argument; tell the client about it.
            (*req).rq_msg.ru.RM_rmb.ru.RP_ar.ru.AR_results.proc_ = Some(xdr_void);
            (*req).rq_msg.ru.RM_rmb.ru.RP_ar.ru.AR_results.where_ = std::ptr::null_mut();
            svc_sendreply(req);
            return xprt_stat_XPRT_IDLE;
        }
        let name_c = std::ffi::CStr::from_ptr(name);
        let reply_c = CString::new(format!("Hello, {}!", name_c.to_string_lossy())).unwrap();
        libc::free(name as *mut c_void);

        // Prepare the reply. svc_sendreply() encodes the result synchronously,
        // so pointing at the reply string on the stack is fine.
        let mut reply_ptr: *mut c_char = reply_c.as_ptr() as *mut c_char;
        (*req).rq_msg.ru.RM_rmb.ru.RP_ar.ru.AR_results.proc_ = Some(wrap_string);
        (*req).rq_msg.ru.RM_rmb.ru.RP_ar.ru.AR_results.where_ =
            &mut reply_ptr as *mut *mut c_char as *mut c_void;

        svc_sendreply(req);
        xprt_stat_XPRT_IDLE
    }
}

/// Called by ntirpc when the first datagram arrives on the listening socket
/// (and a new per-peer transport has been allocated). Hook up the request
/// handler for that transport and process the buffered datagram.
unsafe extern "C" fn udp_rendezvous(xprt: *mut SVCXPRT) -> xprt_stat {
    unsafe {
        (*xprt).xp_dispatch.__bindgen_anon_1.process_cb = Some(hello_process);
        let recv = (*(*xprt).xp_ops).xp_recv;
        match recv {
            Some(f) => f(xprt),
            None => xprt_stat_XPRT_DIED,
        }
    }
}

fn main() {
    unsafe {
        // Start the service's thread pool and request handlers.
        // SVC_INIT_EPOLL must be set or max_events stays 0 and the event
        // loop that handles incoming requests breaks with EINVAL. The
        // request alloc/free callbacks are also required.
        let mut params: svc_init_params = std::mem::zeroed();
        params.flags = SVC_INIT_EPOLL as u_long;
        params.max_events = 16;
        params.alloc_cb = Some(svc_req_alloc);
        params.free_cb = Some(svc_req_free);
        if !svc_init(&mut params) {
            eprintln!("svc_init failed");
            std::process::exit(1);
        }

        // Create a UDP socket bound to an ephemeral port.
        let fd = libc::socket(AF_INET, SOCK_DGRAM, 0);
        if fd < 0 {
            eprintln!("socket() failed: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }
        let addr = libc::sockaddr_in {
            sin_family: AF_INET as libc::sa_family_t,
            sin_port: 0,
            sin_addr: libc::in_addr {
                s_addr: u32::from_ne_bytes(INADDR_ANY.to_ne_bytes()),
            },
            sin_zero: [0; 8],
        };
        let rc = libc::bind(
            fd,
            &addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        if rc != 0 {
            eprintln!("bind() failed: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }

        // Wrap the socket in a UDP rendezvous transport. ntirpc registers it
        // with its event channel, so worker threads call udp_rendezvous
        // whenever a datagram arrives.
        let xprt = svc_dg_ncreatef(fd, 0, 0, SVC_CREATE_FLAG_CLOSE);
        if xprt.is_null() {
            eprintln!("svc_dg_ncreatef failed");
            std::process::exit(1);
        }

        // ntirpc does not install this itself: without a rendezvous callback
        // the first incoming datagram calls a NULL function pointer inside
        // svc_dg_rendezvous() and crashes.
        (*xprt).xp_dispatch.rendezvous_cb = Some(udp_rendezvous);

        // Register the program with rpcbind so clients can find our port.
        // The dispatch argument is only used for bookkeeping; ntirpc routes
        // calls to process_cb, never to this function.
        let nettype = c"udp";
        let nconf = getnetconfigent(nettype.as_ptr());
        if nconf.is_null() {
            eprintln!("getnetconfigent failed");
            std::process::exit(1);
        }
        // rpcbind keeps the mapping of a killed server around, and it will not
        // let a new process overwrite it. Clear any stale entry first.
        rpcb_unset(HELLO_PROG, HELLO_VERS, nconf);
        if !svc_reg(xprt, HELLO_PROG, HELLO_VERS, Some(hello_dispatch), nconf) {
            eprintln!("svc_reg failed (is rpcbind running?)");
            std::process::exit(1);
        }
        freenetconfigent(nconf);

        println!("hello server: prog={HELLO_PROG:#x} vers={HELLO_VERS} on udp");

        // The service runs on its own threads; just keep the process alive.
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}
