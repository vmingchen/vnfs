#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    libntirpc_sys::fuzzing::rpc_xdr(data);
});
