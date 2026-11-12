#![cfg_attr(
    test,
    allow(clippy::undocumented_unsafe_blocks, unsafe_op_in_unsafe_fn)
)]
//! Peer-controlled directory lists must not consume one stack frame per entry.
use nfsv41_sys::*;
use std::process::Command;

const ENTRIES: usize = 2048;

fn wire_reply() -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in [0_u32, 0, 1, nfs_opnum4_NFS4_OP_READDIR, 0, 0, 0] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    for index in 0..ENTRIES {
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(&(index as u64 + 1).to_be_bytes());
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(b"x\0\0\0");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
    }
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    bytes.extend_from_slice(&1_u32.to_be_bytes());
    bytes
}

unsafe fn decode(bytes: &mut [u8], length: usize) -> (bool, Box<COMPOUND4res>) {
    let mut stream: XDR = std::mem::zeroed();
    xdrmem_ncreate(
        &mut stream,
        bytes.as_mut_ptr().cast(),
        length as u32,
        xdr_op_XDR_DECODE,
    );
    let mut reply: Box<COMPOUND4res> = Box::new(std::mem::zeroed());
    let success = xdr_wrap_COMPOUND4res(&mut stream, reply.as_mut());
    (success, reply)
}

unsafe fn free(mut reply: Box<COMPOUND4res>) {
    assert!(xdr_wrap_COMPOUND4res(
        &raw mut xdr_free_null_stream,
        reply.as_mut()
    ));
}

unsafe fn check(reply: &COMPOUND4res) {
    assert_eq!(reply.resarray.resarray_len, 1);
    let listing = (*reply.resarray.resarray_val)
        .nfs_resop4_u
        .opreaddir
        .READDIR4res_u
        .resok4
        .reply;
    assert_eq!(listing.eof, 1);
    let mut entry = listing.entries;
    for index in 0..ENTRIES {
        assert!(!entry.is_null());
        assert_eq!((*entry).cookie, index as u64 + 1);
        assert_eq!((*entry).name.utf8string_len, 1);
        assert_eq!(*(*entry).name.utf8string_val, b'x' as _);
        entry = (*entry).nextentry;
    }
    assert!(entry.is_null());
}

fn child_or_run(name: &str, run: impl FnOnce()) {
    if std::env::var("VFSI_CODEC_STACK_CHILD").as_deref() == Ok(name) {
        run();
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("VFSI_CODEC_STACK_CHILD", name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated codec check failed: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn deep_readdir_decode_uses_bounded_stack() {
    child_or_run("deep_readdir_decode_uses_bounded_stack", || {
        std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(|| unsafe {
                let mut bytes = wire_reply();
                let length = bytes.len();
                let (success, mut reply) = decode(&mut bytes, length);
                assert!(success);
                check(&reply);
                let mut encoded = vec![0_u8; length];
                let mut stream: XDR = std::mem::zeroed();
                xdrmem_ncreate(
                    &mut stream,
                    encoded.as_mut_ptr().cast(),
                    length as u32,
                    xdr_op_XDR_ENCODE,
                );
                assert!(xdr_wrap_COMPOUND4res(&mut stream, reply.as_mut()));
                assert_eq!(
                    stream.x_data.offset_from(stream.x_v.vio_base) as usize,
                    length
                );
                assert_eq!(encoded, bytes);
                free(reply);
            })
            .unwrap()
            .join()
            .unwrap();
    });
}

#[test]
fn deep_readdir_free_uses_bounded_stack() {
    child_or_run("deep_readdir_free_uses_bounded_stack", || unsafe {
        let mut bytes = wire_reply();
        let length = bytes.len();
        let (success, reply) = decode(&mut bytes, length);
        assert!(success);
        check(&reply);
        // Transfer exclusive ownership into the cleanup thread. COMPOUND4res
        // contains raw pointers, but no references into this thread's stack.
        let address = Box::into_raw(reply) as usize;
        std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || free(Box::from_raw(address as *mut COMPOUND4res)))
            .unwrap()
            .join()
            .unwrap();
    });
}

#[test]
fn truncated_deep_readdir_can_be_freed() {
    // This executable's other tests isolate cleanup in their own processes.
    unsafe {
        let mut bytes = wire_reply();
        for length in [
            28,
            29,
            32,
            40,
            52,
            bytes.len() - 9,
            bytes.len() - 5,
            bytes.len() - 1,
        ] {
            let (success, reply) = decode(&mut bytes, length);
            assert!(!success, "truncated reply of {length} bytes");
            free(reply);
        }
    }
}
