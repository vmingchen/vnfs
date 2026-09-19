#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    vfsi_nfs::fuzzing::compound_response(data);
});
