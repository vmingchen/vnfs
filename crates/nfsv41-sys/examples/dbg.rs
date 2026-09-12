use nfsv41_sys::*;
use std::os::raw::c_char;

fn main() {
    unsafe {
        let fh: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];
        let tag = b"roundtrip\0".to_vec();
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

        let mut xdr: XDR = std::mem::zeroed();
        let mut buf = vec![0u8; 4096];
        xdrmem_ncreate(
            &mut xdr,
            buf.as_mut_ptr() as *mut c_char,
            4096,
            xdr_op_XDR_ENCODE,
        );
        let ok = xdr_wrap_COMPOUND4args(&mut xdr, &mut args);
        println!("encode ok: {}", ok);
        let len = xdr.x_data.offset_from(xdr.x_v.vio_base) as usize;
        println!("len: {}", len);
        println!("hex: {:02x?}", &buf[0..len]);

        let mut xdr2: XDR = std::mem::zeroed();
        let mut b2 = buf[0..len].to_vec();
        xdrmem_ncreate(
            &mut xdr2,
            b2.as_mut_ptr() as *mut c_char,
            len as u32,
            xdr_op_XDR_DECODE,
        );
        let mut decoded: COMPOUND4args = std::mem::zeroed();
        let ok2 = xdr_wrap_COMPOUND4args(&mut xdr2, &mut decoded);
        println!("decode ok: {}", ok2);
        if ok2 {
            println!(
                "tag len {}: {}",
                decoded.tag.utf8string_len,
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    decoded.tag.utf8string_val as *const u8,
                    decoded.tag.utf8string_len as usize
                ))
            );
            println!("minorversion: {}", decoded.minorversion);
            println!("nargs: {}", decoded.argarray.argarray_len);
            let da = &*decoded.argarray.argarray_val;
            println!("argop: {}", da.argop);
            println!("fh len: {}", da.nfs_argop4_u.opputfh.object.nfs_fh4_len);
            println!(
                "fh: {:02x?}",
                std::slice::from_raw_parts(
                    da.nfs_argop4_u.opputfh.object.nfs_fh4_val as *const u8,
                    da.nfs_argop4_u.opputfh.object.nfs_fh4_len as usize
                )
            );
            xdr_wrap_COMPOUND4args(&raw mut xdr_free_null_stream, &mut decoded);
        }
    }
}
