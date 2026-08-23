//! Round-trip tests: encode COMPOUND4args / COMPOUND4res (one PUTFH op)
//! with the nfsv41.h codecs through a libntirpc memory XDR, decode them
//! back, and verify the fields match.

#![allow(unsafe_op_in_unsafe_fn)]
use nfsv41_sys::*;
use std::os::raw::c_char;

const BUF_SIZE: usize = 4096;

unsafe fn encode_compound(args: &mut COMPOUND4args) -> Vec<u8> {
    let mut xdr: XDR = std::mem::zeroed();
    let mut buf = vec![0u8; BUF_SIZE];
    xdrmem_ncreate(
        &mut xdr,
        buf.as_mut_ptr() as *mut c_char,
        BUF_SIZE as u32,
        xdr_op_XDR_ENCODE,
    );
    let ok = xdr_wrap_COMPOUND4args(&mut xdr, args);
    assert!(ok, "xdr_COMPOUND4args encode failed");

    // xdrmem's put path advances XDR.x_data; vio_base is the buffer start.
    let len = xdr.x_data.offset_from(xdr.x_v.vio_base) as usize;
    assert!(len > 0 && len <= BUF_SIZE);
    buf.truncate(len);
    buf
}

unsafe fn encode_res(res: &mut COMPOUND4res) -> Vec<u8> {
    let mut xdr: XDR = std::mem::zeroed();
    let mut buf = vec![0u8; BUF_SIZE];
    xdrmem_ncreate(
        &mut xdr,
        buf.as_mut_ptr() as *mut c_char,
        BUF_SIZE as u32,
        xdr_op_XDR_ENCODE,
    );
    let ok = xdr_wrap_COMPOUND4res(&mut xdr, res);
    assert!(ok, "xdr_COMPOUND4res encode failed");

    let len = xdr.x_data.offset_from(xdr.x_v.vio_base) as usize;
    assert!(len > 0 && len <= BUF_SIZE);
    buf.truncate(len);
    buf
}

#[test]
fn compound_putfh_args_roundtrip() {
    unsafe {
        let fh: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];
        let tag = b"roundtrip\0".to_vec();

        // Keep argop in this frame for the whole encode: the codecs read
        // the array contents through the raw pointer below.
        let mut argop: nfs_argop4 = std::mem::zeroed();
        argop.argop = nfs_opnum4_NFS4_OP_PUTFH;
        argop.nfs_argop4_u.opputfh.object.nfs_fh4_len = fh.len() as u32;
        argop.nfs_argop4_u.opputfh.object.nfs_fh4_val = fh.as_ptr() as *mut c_char;

        let mut args: COMPOUND4args = std::mem::zeroed();
        args.tag.utf8string_len = tag.len() as u32;
        args.tag.utf8string_val = tag.as_ptr() as *mut c_char;
        args.minorversion = 1;
        args.argarray.argarray_len = 1;
        args.argarray.argarray_val = &mut argop as *mut nfs_argop4;

        let encoded = encode_compound(&mut args);
        // Layout: tag_len(4) tag(10) pad(2) minorversion(4) nops(4)
        // op(4) fh_len(4) fh(4). XDR pads the 10-byte tag to 12.
        assert_eq!(encoded.len(), 36);
        assert_eq!(&encoded[0..4], [0x00, 0x00, 0x00, 0x0a]); // tag length, big-endian
        assert_eq!(&encoded[4..14], tag.as_slice());
        assert_eq!(&encoded[16..20], [0x00, 0x00, 0x00, 0x01]); // minorversion
        assert_eq!(&encoded[20..24], [0x00, 0x00, 0x00, 0x01]); // nops

        // Decode back into COMPOUND4args. The codecs allocate the decoded
        // arrays/strings via mem_zalloc.
        let mut xdr: XDR = std::mem::zeroed();
        let mut buf = encoded;
        xdrmem_ncreate(
            &mut xdr,
            buf.as_mut_ptr() as *mut c_char,
            buf.len() as u32,
            xdr_op_XDR_DECODE,
        );
        let mut decoded: COMPOUND4args = std::mem::zeroed();
        assert!(xdr_wrap_COMPOUND4args(&mut xdr, &mut decoded));

        assert_eq!(decoded.tag.utf8string_len, tag.len() as u32);
        let dtag = std::slice::from_raw_parts(
            decoded.tag.utf8string_val as *const u8,
            decoded.tag.utf8string_len as usize,
        );
        assert_eq!(dtag, tag.as_slice());

        assert_eq!(decoded.minorversion, 1);
        assert_eq!(decoded.argarray.argarray_len, 1);
        let dargop = &*decoded.argarray.argarray_val;
        assert_eq!(dargop.argop, nfs_opnum4_NFS4_OP_PUTFH);
        assert_eq!(
            dargop.nfs_argop4_u.opputfh.object.nfs_fh4_len,
            fh.len() as u32
        );
        let dfh = std::slice::from_raw_parts(
            dargop.nfs_argop4_u.opputfh.object.nfs_fh4_val as *const u8,
            dargop.nfs_argop4_u.opputfh.object.nfs_fh4_len as usize,
        );
        assert_eq!(dfh, fh.as_slice());

        // Free the decoded tree with the xdr_free_null_stream trick.
        xdr_wrap_COMPOUND4args(&raw mut xdr_free_null_stream, &mut decoded);
    }
}

#[test]
fn compound_putfh_res_roundtrip() {
    unsafe {
        let tag = b"roundtrip\0".to_vec();

        let mut resop: nfs_resop4 = std::mem::zeroed();
        resop.resop = nfs_opnum4_NFS4_OP_PUTFH;
        resop.nfs_resop4_u.opputfh.status = nfsstat4_NFS4_OK;

        let mut res: COMPOUND4res = std::mem::zeroed();
        res.status = nfsstat4_NFS4_OK;
        res.tag.utf8string_len = tag.len() as u32;
        res.tag.utf8string_val = tag.as_ptr() as *mut c_char;
        res.resarray.resarray_len = 1;
        res.resarray.resarray_val = &mut resop as *mut nfs_resop4;

        let encoded = encode_res(&mut res);
        // Layout: status(4) tag_len(4) tag(10) pad(2) nres(4) op(4) res status(4).
        assert_eq!(encoded.len(), 32);
        assert_eq!(&encoded[0..4], [0x00, 0x00, 0x00, 0x00]); // NFS4_OK
        assert_eq!(&encoded[4..8], [0x00, 0x00, 0x00, 0x0a]); // tag length
        assert_eq!(&encoded[8..18], tag.as_slice());
        assert_eq!(&encoded[20..24], [0x00, 0x00, 0x00, 0x01]); // nres

        let mut xdr: XDR = std::mem::zeroed();
        let mut buf = encoded;
        xdrmem_ncreate(
            &mut xdr,
            buf.as_mut_ptr() as *mut c_char,
            buf.len() as u32,
            xdr_op_XDR_DECODE,
        );
        let mut decoded: COMPOUND4res = std::mem::zeroed();
        assert!(xdr_wrap_COMPOUND4res(&mut xdr, &mut decoded));

        assert_eq!(decoded.status, nfsstat4_NFS4_OK);
        assert_eq!(decoded.tag.utf8string_len, tag.len() as u32);
        let dtag = std::slice::from_raw_parts(
            decoded.tag.utf8string_val as *const u8,
            decoded.tag.utf8string_len as usize,
        );
        assert_eq!(dtag, tag.as_slice());

        assert_eq!(decoded.resarray.resarray_len, 1);
        let dresop = &*decoded.resarray.resarray_val;
        assert_eq!(dresop.resop, nfs_opnum4_NFS4_OP_PUTFH);
        assert_eq!(dresop.nfs_resop4_u.opputfh.status, nfsstat4_NFS4_OK);

        xdr_wrap_COMPOUND4res(&raw mut xdr_free_null_stream, &mut decoded);
    }
}
