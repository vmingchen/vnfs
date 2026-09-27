//! Safe adapters that exercise libntirpc's XDR and ONC RPC decoders with
//! untrusted bytes.
//!
//! This module is available only with the `fuzzing` feature. These are the
//! same decoders libntirpc runs on data received from the network, so they
//! must tolerate hostile input. Every structure and allocation that crosses
//! the C ABI is owned here, which keeps a fuzz target from reaching libntirpc
//! with an uninitialized pointer.

use std::os::raw::{c_char, c_void};
use std::ptr;

use crate::{
    XDR, rpc_msg, u_int, xdr_ncallmsg, xdr_nreplymsg, xdr_op_XDR_DECODE, xdr_void, xdr_wrapstring,
    xdrmem_ncreate,
};

/// Decode `data` as an RPC call message, an RPC reply message, and a counted
/// XDR string.
///
/// libntirpc reports malformed input by returning `false` from these
/// routines; no input may crash, abort, or leak.
pub fn rpc_xdr(data: &[u8]) {
    decode_call(data);
    decode_reply(data);
    decode_string(data);
}

fn memory_stream(data: &[u8]) -> (Vec<u32>, XDR) {
    // `xdrmem_ncreate` aborts unless the backing store is 32-bit aligned, and
    // a `Vec<u8>` only guarantees one-byte alignment. Storing the bytes in a
    // `Vec<u32>` gives the required alignment while keeping the exact byte
    // sequence the decoder expects.
    let words = data.len().div_ceil(4);
    let mut buffer = vec![0u32; words];
    if !data.is_empty() {
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(buffer.as_mut_ptr().cast::<u8>(), words * 4) };
        bytes[..data.len()].copy_from_slice(data);
    }
    let mut xdrs: XDR = unsafe { std::mem::zeroed() };
    // `xdrmem_ncreate` takes a 32-bit length; never let a larger slice wrap.
    let length = data.len().min(u_int::MAX as usize) as u_int;
    unsafe {
        xdrmem_ncreate(
            &mut xdrs,
            buffer.as_mut_ptr().cast::<c_char>(),
            length,
            xdr_op_XDR_DECODE,
        );
    }
    (buffer, xdrs)
}

/// Decode a CALL message. `xdr_ncallmsg` writes only into the caller-provided
/// `rpc_msg` (the credential and verifier use inline fixed-size storage), so
/// `XDR_FREE` is a no-op and nothing outlives the call.
fn decode_call(data: &[u8]) {
    let (_buffer, mut xdrs) = memory_stream(data);
    let mut msg: rpc_msg = unsafe { std::mem::zeroed() };
    let ok = unsafe { xdr_ncallmsg(&mut xdrs, &mut msg) };
    std::hint::black_box((ok, &msg));
}

/// Decode a REPLY message. On `accept_stat == SUCCESS` the decoder invokes
/// `AR_results.proc`, exactly as a real client does, so install a no-op result
/// decoder before handing over the message.
fn decode_reply(data: &[u8]) {
    let (_buffer, mut xdrs) = memory_stream(data);
    let mut msg: rpc_msg = unsafe { std::mem::zeroed() };
    msg.ru.RM_rmb.ru.RP_ar.ru.AR_results.proc_ = Some(xdr_void);
    msg.ru.RM_rmb.ru.RP_ar.ru.AR_results.where_ = ptr::null_mut::<c_void>();
    let ok = unsafe { xdr_nreplymsg(&mut xdrs, &mut msg) };
    std::hint::black_box((ok, &msg));
}

/// Decode a counted string and release it through libntirpc's own free stream
/// so the `mem_alloc`/`mem_free` allocator pair stays matched. The declared
/// length is attacker-controlled but `xdr_wrapstring` caps it at
/// `RPC_MAXDATASIZE`.
fn decode_string(data: &[u8]) {
    let (_buffer, mut xdrs) = memory_stream(data);
    let mut string: *mut c_char = ptr::null_mut();
    if unsafe { xdr_wrapstring(&mut xdrs, &mut string) } && !string.is_null() {
        unsafe {
            xdr_wrapstring(&raw mut crate::xdr_free_null_stream, &mut string);
        }
    }
}
