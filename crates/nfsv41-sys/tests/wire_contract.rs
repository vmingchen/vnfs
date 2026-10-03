//! Independent wire fixtures catch encoder/decoder bugs hidden by round trips.
use nfsv41_sys::*;
use std::os::raw::c_char;
mod support;

// COMPOUND: empty tag, minor=1, one PUTFH (op 22), three-byte handle + padding.
const PUTFH: [u8; 24] = [
    0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 22, 0, 0, 0, 3, 0xaa, 0xbb, 0xcc, 0,
];

#[test]
fn putfh_encoding_matches_an_independent_wire_fixture() {
    unsafe {
        let mut handle = [0xaa_u8, 0xbb, 0xcc];
        let mut op: nfs_argop4 = std::mem::zeroed();
        op.argop = nfs_opnum4_NFS4_OP_PUTFH;
        op.nfs_argop4_u.opputfh.object.nfs_fh4_len = 3;
        op.nfs_argop4_u.opputfh.object.nfs_fh4_val = handle.as_mut_ptr().cast();
        let mut args: COMPOUND4args = std::mem::zeroed();
        args.minorversion = 1;
        args.argarray.argarray_len = 1;
        args.argarray.argarray_val = &mut op;
        assert_eq!(support::encode_compound(&mut args), PUTFH);
    }
}

#[test]
fn putfh_decoder_rejects_every_truncated_wire_prefix() {
    unsafe {
        for length in 0..=PUTFH.len() {
            // Keep allocated, aligned storage even for the empty prefix:
            // an empty Vec<u8>'s dangling pointer is not a valid XDR buffer.
            // Only `length` bytes are exposed to the decoder.
            let mut bytes = PUTFH.to_vec();
            let mut xdr: XDR = std::mem::zeroed();
            xdrmem_ncreate(
                &mut xdr,
                bytes.as_mut_ptr() as *mut c_char,
                length as u32,
                xdr_op_XDR_DECODE,
            );
            let mut args: COMPOUND4args = std::mem::zeroed();
            let success = xdr_wrap_COMPOUND4args(&mut xdr, &mut args);
            if success {
                assert_eq!(args.minorversion, 1);
                assert_eq!(args.argarray.argarray_len, 1);
                let handle = (*args.argarray.argarray_val).nfs_argop4_u.opputfh.object;
                assert_eq!(
                    std::slice::from_raw_parts(
                        handle.nfs_fh4_val.cast::<u8>(),
                        handle.nfs_fh4_len as usize
                    ),
                    &[0xaa, 0xbb, 0xcc]
                );
            }
            // Decode can allocate a partial tree before rejecting truncation.
            xdr_wrap_COMPOUND4args(&raw mut xdr_free_null_stream, &mut args);
            assert_eq!(
                success,
                length == PUTFH.len(),
                "wire prefix length {length}"
            );
        }
    }
}

#[test]
fn putfh_failure_reply_uses_the_error_union_arm() {
    unsafe {
        // Overall NOENT, empty tag, one PUTFH result, NOENT. There is no
        // success payload to encode in this operation's error arm.
        let golden = [0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 22, 0, 0, 0, 2];
        let mut op: nfs_resop4 = std::mem::zeroed();
        op.resop = nfs_opnum4_NFS4_OP_PUTFH;
        op.nfs_resop4_u.opputfh.status = nfsstat4_NFS4ERR_NOENT;
        let mut reply: COMPOUND4res = std::mem::zeroed();
        reply.status = nfsstat4_NFS4ERR_NOENT;
        reply.resarray.resarray_len = 1;
        reply.resarray.resarray_val = &mut op;
        assert_eq!(support::encode_res(&mut reply), golden);
    }
}
