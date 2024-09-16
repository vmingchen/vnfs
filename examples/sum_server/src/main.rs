use std::os::raw::c_char;
use sum_proto::{SUM_PROGRAM, SUM_VERSION};
use libtirpc_sys as tirpc;

fn main() {
    // SAFETY: FFI
    unsafe {
        let nc_tcp = tirpc::NC_TCP.as_ptr() as *const c_char;
        let netconfig = tirpc::getnetconfigent(nc_tcp);
        if netconfig.is_null() {
            panic!("tirpc::getnetconfigent failed");
        }
        tirpc::rpcb_unset(SUM_PROGRAM, SUM_VERSION, netconfig);

        let xprt = tirpc::svc_tp_ncreate(Some(sum_svc), SUM_PROGRAM, SUM_VERSION, netconfig);
        if xprt.is_null() {
            panic!("tirpc::svc_tp_ncreate failed");
        }

        tirpc::freenetconfigent(netconfig);

        tirpc::svc_run(); // never return
    }
}
