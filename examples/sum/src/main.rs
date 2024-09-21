use std::{env, ffi::{c_void, CString}, process::abort};

fn main() {
    let (host, a, b) = if env::args().count() == 3 {
        let a = env::args().nth(1).unwrap().parse::<i32>().expect("a");
        let b = env::args().nth(2).unwrap().parse::<i32>().expect("b");
        (CString::new("localhost"), a, b)
    } else if env::args().count() == 4 {
        let host = env::args().nth(1).expect("host");
        let a = env::args().nth(2).unwrap().parse::<i32>().expect("a");
        let b = env::args().nth(3).unwrap().parse::<i32>().expect("b");
        (CString::new(host), a, b)
    } else {
        panic!("Usage: 'sum <a> <b>' OR 'sum <host> <a> <b>'")
    };

    let args = Box::new(sum_lib::sum_args { a, b });

    let host = Box::new(host.unwrap());

    const SUMPROC : sum_lib::rpcproc_t = 1;

    unsafe {
        let clnt = sum_lib::clnt_create(
            host.as_ptr(),
            sum_lib::SUMPROG,
            sum_lib::SUMVERS,
            c"udp".as_ptr(),
        );

        if clnt.is_null() {
            panic!("clnt_create failed");
        }

        let args = Box::into_raw(args);

        let res = Box::new(0);
        let res: *mut i32 = Box::into_raw(res);

        let clnt_ops = (*clnt).cl_ops;

        let call_fn = (*clnt_ops).cl_call.expect("cl_call");

        let timeout = sum_lib::timeval {
            tv_sec: 25,
            tv_usec: 0,
        };

        let arg_xdr: unsafe extern "C" fn(*mut sum_lib::XDR, ...) -> i32 =
            std::mem::transmute(sum_lib::xdr_sum_args as usize);

        let res_xdr: unsafe extern "C" fn(*mut sum_lib::XDR, ...) -> i32 =
            std::mem::transmute(sum_lib::xdr_int as usize);

        let rpc_res = call_fn(
            clnt,
            SUMPROC,
            Some(arg_xdr),
            args as *mut c_void,
            Some(res_xdr),
            res as *mut c_void,
            timeout,
        );
        if rpc_res != 0 { 
            sum_lib::clnt_perror(clnt, c"clnt_call failed".as_ptr());
            abort();
        }

        let res = sum_lib::sum_1(args, clnt);
        if res.is_null() {
            sum_lib::clnt_perror(clnt, c"sum_1 failed".as_ptr());
            abort();
        } else {
            println!("{} + {} = {}", a, b, *res);
        }

        // clean up
        let _ = Box::from_raw(args);
        // `res`` will be freed by `clnt_destroy`
        let destroy_fn = (*clnt_ops).cl_destroy.expect("cl_destroy");
        destroy_fn(clnt);
    }
}
