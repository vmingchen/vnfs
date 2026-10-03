#![allow(unsafe_op_in_unsafe_fn)]
use nfsv41_sys::*;
use std::os::raw::c_char;

unsafe fn encode<T>(
    value: &mut T,
    codec: unsafe extern "C" fn(*mut XDR, *mut T) -> bool,
) -> Vec<u8> {
    let mut xdr: XDR = std::mem::zeroed();
    let mut buffer = vec![0; 8192];
    xdrmem_ncreate(
        &mut xdr,
        buffer.as_mut_ptr() as *mut c_char,
        buffer.len() as u32,
        xdr_op_XDR_ENCODE,
    );
    assert!(codec(&mut xdr, value), "XDR encoding failed");
    let length = xdr.x_data.offset_from(xdr.x_v.vio_base) as usize;
    assert!(length > 0 && length <= buffer.len());
    buffer.truncate(length);
    buffer
}

pub unsafe fn encode_compound(value: &mut COMPOUND4args) -> Vec<u8> {
    encode(value, xdr_wrap_COMPOUND4args)
}

pub unsafe fn encode_res(value: &mut COMPOUND4res) -> Vec<u8> {
    encode(value, xdr_wrap_COMPOUND4res)
}
