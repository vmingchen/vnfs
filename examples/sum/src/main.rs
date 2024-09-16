use std::{env, ffi::CStr};

use clap::{Parser, Subcommand};
#[allow(non_camel_case_types)]
use libntirpc_sys as ntirpc;

const PROGRAM : ntirpc::rpcprog_t = 22888;
const VERSION : ntirpc::rpcvers_t = 1;
const PORT: ntirpc::in_port_t = 8088;

// SAFETY: FFI
const HOST : &'static CStr = unsafe {
    CStr::from_bytes_with_nul_unchecked(b"localhost\0")
};

#[no_mangle]
unsafe extern "C" fn sum_svc(svc_req: *mut ntirpc::svc_req) {
}

fn serve() {
    // SAFETY: FFI
    unsafe {
        let nc_tcp = ntirpc::NC_TCP.as_ptr() as *const i8;
        let netconfig = ntirpc::getnetconfigent(nc_tcp);
        if netconfig.is_null() {
            panic!("ntirpc::getnetconfigent failed");
        }
        ntirpc::rpcb_unset(PROGRAM, VERSION, netconfig);

        let xprt = ntirpc::svc_tp_ncreate(Some(sum_svc), PROGRAM, VERSION, netconfig);
        if xprt.is_null() {
            panic!("ntirpc::svc_tp_ncreate failed");
        }

        ntirpc::freenetconfigent(netconfig);
    }
}
    // ntirpc::clnt_ncreate_timed()
    // ntirpc::svc_tp_ncreate(arg1, arg2, arg3, arg4)

fn call(a: i32, b: i32) -> i32 {
    a + b
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: SumCommand,
}

#[derive(Clone, Subcommand)]
enum SumCommand {
    Serve,
    Call{a: i32, b: i32},
}

fn main() {
    let cli = Cli::parse();

    match &cli.command {
        SumCommand::Serve => serve(),
        SumCommand::Call{a, b} => println!("{}", call(*a, *b)),
    }
}
