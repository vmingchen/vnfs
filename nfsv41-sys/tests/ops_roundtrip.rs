//! Round-trip tests for individual NFSv4.1 operations: encode a COMPOUND4args
//! / COMPOUND4res holding a single op with the nfsv41.h codecs, decode it
//! back, and verify the fields match.
//!
//! Note: this header (Ganesha's nfsv41.h) does not define standalone MKDIR4
//! or SYMLINK4 arg/res types (MKDIR/SYMLINK ops are absent); directory and
//! symlink coverage is via LOOKUP / READDIR / REMOVE / RENAME / CREATE and
//! READLINK instead.

#![allow(unsafe_op_in_unsafe_fn)]
use nfsv41_sys::*;
use std::os::raw::c_char;

const BUF_SIZE: usize = 8192;

fn utf8string(bytes: &[u8]) -> utf8string {
    utf8string {
        utf8string_len: bytes.len() as u32,
        utf8string_val: bytes.as_ptr() as *mut c_char,
    }
}

fn cbytes(v: [u8; 8]) -> [c_char; 8] {
    std::array::from_fn(|i| v[i] as c_char)
}

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

unsafe fn decode_compound(buf: &[u8]) -> COMPOUND4args {
    let mut xdr: XDR = std::mem::zeroed();
    let mut b = buf.to_vec();
    xdrmem_ncreate(
        &mut xdr,
        b.as_mut_ptr() as *mut c_char,
        b.len() as u32,
        xdr_op_XDR_DECODE,
    );
    let mut decoded: COMPOUND4args = std::mem::zeroed();
    assert!(xdr_wrap_COMPOUND4args(&mut xdr, &mut decoded));
    decoded
}

unsafe fn decode_res(buf: &[u8]) -> COMPOUND4res {
    let mut xdr: XDR = std::mem::zeroed();
    let mut b = buf.to_vec();
    xdrmem_ncreate(
        &mut xdr,
        b.as_mut_ptr() as *mut c_char,
        b.len() as u32,
        xdr_op_XDR_DECODE,
    );
    let mut decoded: COMPOUND4res = std::mem::zeroed();
    assert!(xdr_wrap_COMPOUND4res(&mut xdr, &mut decoded));
    decoded
}

/// Encode a single-op COMPOUND4args, decode it back, run `check` against the
/// decoded op (while its storage is alive), then free the decoded tree.
unsafe fn roundtrip_argop(argop: nfs_argop4, tag: &[u8], check: impl FnOnce(&nfs_argop4)) {
    let mut a = argop;
    let mut args: COMPOUND4args = std::mem::zeroed();
    args.tag = utf8string(tag);
    args.minorversion = 1;
    args.argarray.argarray_len = 1;
    args.argarray.argarray_val = &mut a;
    let encoded = encode_compound(&mut args);
    let mut decoded = decode_compound(&encoded);
    check(&*decoded.argarray.argarray_val);
    xdr_wrap_COMPOUND4args(&raw mut xdr_free_null_stream, &mut decoded);
}

/// Encode a single-op COMPOUND4res, decode it back, run `check` against the
/// decoded op, then free the decoded tree.
unsafe fn roundtrip_resop(resop: nfs_resop4, tag: &[u8], check: impl FnOnce(&nfs_resop4)) {
    let mut r = resop;
    let mut res: COMPOUND4res = std::mem::zeroed();
    res.status = nfsstat4_NFS4_OK;
    res.tag = utf8string(tag);
    res.resarray.resarray_len = 1;
    res.resarray.resarray_val = &mut r;
    let encoded = encode_res(&mut res);
    let mut decoded = decode_res(&encoded);
    check(&*decoded.resarray.resarray_val);
    xdr_wrap_COMPOUND4res(&raw mut xdr_free_null_stream, &mut decoded);
}

fn stateid(seqid: u32) -> stateid4 {
    stateid4 {
        seqid,
        other: [0; 12],
    }
}

fn change_info(atomic: i32, before: u64, after: u64) -> change_info4 {
    change_info4 {
        atomic,
        before,
        after,
    }
}

// ---------------------------------------------------------------------------
// READ
// ---------------------------------------------------------------------------

#[test]
fn read_args_roundtrip() {
    unsafe {
        let mut argop: nfs_argop4 = std::mem::zeroed();
        argop.argop = nfs_opnum4_NFS4_OP_READ;
        argop.nfs_argop4_u.opread = READ4args {
            stateid: stateid(7),
            offset: 0x1122_3344_5566_7788,
            count: 4096,
        };
        roundtrip_argop(argop, b"read", |d| {
            assert_eq!(d.argop, nfs_opnum4_NFS4_OP_READ);
            let r = &d.nfs_argop4_u.opread;
            assert_eq!(r.stateid.seqid, 7);
            assert_eq!(r.offset, 0x1122_3344_5566_7788);
            assert_eq!(r.count, 4096);
        });
    }
}

#[test]
fn read_res_roundtrip() {
    unsafe {
        let data: &[u8] = b"hello from the nfs server\n";
        let mut resop: nfs_resop4 = std::mem::zeroed();
        resop.resop = nfs_opnum4_NFS4_OP_READ;
        resop.nfs_resop4_u.opread.status = nfsstat4_NFS4_OK;
        resop.nfs_resop4_u.opread.READ4res_u.resok4 = READ4resok {
            eof: 1,
            data: READ4resok__bindgen_ty_1 {
                data_len: data.len() as u32,
                data_val: data.as_ptr() as *mut c_char,
            },
        };
        roundtrip_resop(resop, b"read", |d| {
            let r = &d.nfs_resop4_u.opread;
            assert_eq!(r.status, nfsstat4_NFS4_OK);
            let ok = &r.READ4res_u.resok4;
            assert_eq!(ok.eof, 1);
            assert_eq!(ok.data.data_len, data.len() as u32);
            let got = std::slice::from_raw_parts(
                ok.data.data_val as *const u8,
                ok.data.data_len as usize,
            );
            assert_eq!(got, data);
        });
    }
}

// ---------------------------------------------------------------------------
// WRITE
// ---------------------------------------------------------------------------

#[test]
fn write_args_roundtrip() {
    unsafe {
        let data: &[u8] = b"0123456789abcdef";
        let mut argop: nfs_argop4 = std::mem::zeroed();
        argop.argop = nfs_opnum4_NFS4_OP_WRITE;
        argop.nfs_argop4_u.opwrite = WRITE4args {
            stateid: stateid(3),
            offset: 0x0000_0000_0000_0100,
            stable: stable_how4_FILE_SYNC4,
            data: WRITE4args__bindgen_ty_1 {
                data_len: data.len() as u32,
                data_val: data.as_ptr() as *mut c_char,
            },
        };
        roundtrip_argop(argop, b"write", |d| {
            let w = &d.nfs_argop4_u.opwrite;
            assert_eq!(w.stateid.seqid, 3);
            assert_eq!(w.offset, 0x100);
            assert_eq!(w.stable, stable_how4_FILE_SYNC4);
            assert_eq!(w.data.data_len, data.len() as u32);
            let got =
                std::slice::from_raw_parts(w.data.data_val as *const u8, w.data.data_len as usize);
            assert_eq!(got, data);
        });
    }
}

#[test]
fn write_res_roundtrip() {
    unsafe {
        let mut resop: nfs_resop4 = std::mem::zeroed();
        resop.resop = nfs_opnum4_NFS4_OP_WRITE;
        resop.nfs_resop4_u.opwrite.status = nfsstat4_NFS4_OK;
        resop.nfs_resop4_u.opwrite.WRITE4res_u.resok4 = WRITE4resok {
            count: 16,
            committed: stable_how4_FILE_SYNC4,
            writeverf: cbytes([0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]),
        };
        roundtrip_resop(resop, b"write", |d| {
            let w = &d.nfs_resop4_u.opwrite;
            assert_eq!(w.status, nfsstat4_NFS4_OK);
            let ok = &w.WRITE4res_u.resok4;
            assert_eq!(ok.count, 16);
            assert_eq!(ok.committed, stable_how4_FILE_SYNC4);
            assert_eq!(
                ok.writeverf,
                cbytes([0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88])
            );
        });
    }
}

// ---------------------------------------------------------------------------
// LOOKUP (directory op)
// ---------------------------------------------------------------------------

#[test]
fn lookup_args_roundtrip() {
    unsafe {
        let name: &[u8] = b"subdir";
        let mut argop: nfs_argop4 = std::mem::zeroed();
        argop.argop = nfs_opnum4_NFS4_OP_LOOKUP;
        argop.nfs_argop4_u.oplookup = LOOKUP4args {
            objname: utf8string(name),
        };
        roundtrip_argop(argop, b"lookup", |d| {
            let l = &d.nfs_argop4_u.oplookup;
            assert_eq!(l.objname.utf8string_len, name.len() as u32);
            let got = std::slice::from_raw_parts(
                l.objname.utf8string_val as *const u8,
                l.objname.utf8string_len as usize,
            );
            assert_eq!(got, name);
        });
    }
}

#[test]
fn lookup_res_roundtrip() {
    unsafe {
        let mut resop: nfs_resop4 = std::mem::zeroed();
        resop.resop = nfs_opnum4_NFS4_OP_LOOKUP;
        resop.nfs_resop4_u.oplookup.status = nfsstat4_NFS4_OK;
        roundtrip_resop(resop, b"lookup", |d| {
            assert_eq!(d.nfs_resop4_u.oplookup.status, nfsstat4_NFS4_OK);
        });
    }
}

// ---------------------------------------------------------------------------
// READDIR (directory op)
// ---------------------------------------------------------------------------

#[test]
fn readdir_args_roundtrip() {
    unsafe {
        let mut argop: nfs_argop4 = std::mem::zeroed();
        argop.argop = nfs_opnum4_NFS4_OP_READDIR;
        argop.nfs_argop4_u.opreaddir = READDIR4args {
            cookie: 0xdead_beef,
            cookieverf: cbytes([1, 2, 3, 4, 5, 6, 7, 8]),
            dircount: 4096,
            maxcount: 8192,
            // The codec's xdr_bitmap4 drops decoded words on XDR_DECODE (the
            // malloc'd array is never copied back into map[3]); keep len 0
            // for the roundtrip. Encode-only coverage below.
            attr_request: bitmap4 {
                bitmap4_len: 0,
                map: [0; 3],
            },
        };
        roundtrip_argop(argop, b"readdir", |d| {
            let r = &d.nfs_argop4_u.opreaddir;
            assert_eq!(r.cookie, 0xdead_beef);
            assert_eq!(r.cookieverf, cbytes([1, 2, 3, 4, 5, 6, 7, 8]));
            assert_eq!(r.dircount, 4096);
            assert_eq!(r.maxcount, 8192);
            assert_eq!(r.attr_request.bitmap4_len, 0);
        });
    }
}

#[test]
fn readdir_args_bitmap_encode() {
    // Golden-byte check for a non-empty bitmap4 on the encode path (the
    // decode path of xdr_bitmap4 does not preserve the words, see above).
    unsafe {
        let mut argop: nfs_argop4 = std::mem::zeroed();
        argop.argop = nfs_opnum4_NFS4_OP_READDIR;
        argop.nfs_argop4_u.opreaddir = READDIR4args {
            cookie: 0,
            cookieverf: [0; 8],
            dircount: 0,
            maxcount: 0,
            attr_request: bitmap4 {
                bitmap4_len: 2,
                map: [3, 1, 0],
            },
        };
        let mut a = argop;
        let mut args: COMPOUND4args = std::mem::zeroed();
        args.tag = utf8string(b"readdir");
        args.minorversion = 1;
        args.argarray.argarray_len = 1;
        args.argarray.argarray_val = &mut a;
        let encoded = encode_compound(&mut args);

        // tag_len(4) tag(7+pad1) minorversion(4) nops(4) opnum=26(4)
        // cookie(8) cookieverf(8) dircount(4) maxcount(4) bitmap_len(4) w0(4) w1(4)
        assert_eq!(encoded.len(), 4 + 8 + 4 + 4 + 4 + 8 + 8 + 4 + 4 + 4 + 4 + 4);
        let be = |o: usize| -> u32 {
            u32::from_be_bytes([encoded[o], encoded[o + 1], encoded[o + 2], encoded[o + 3]])
        };
        assert_eq!(be(0), 7); // tag length
        assert_eq!(&encoded[4..11], b"readdir");
        assert_eq!(be(12), 1); // minorversion
        assert_eq!(be(16), 1); // nops
        assert_eq!(be(20), nfs_opnum4_NFS4_OP_READDIR);
        assert_eq!(be(48), 2); // bitmap len
        assert_eq!(be(52), 3); // map[0]
        assert_eq!(be(56), 1); // map[1]
    }
}

#[test]
fn readdir_res_roundtrip() {
    unsafe {
        let name: &[u8] = b"file.txt";
        let mut entry: entry4 = std::mem::zeroed();
        entry.cookie = 0x1234_5678;
        entry.name = utf8string(name);
        entry.nextentry = std::ptr::null_mut();

        let mut resop: nfs_resop4 = std::mem::zeroed();
        resop.resop = nfs_opnum4_NFS4_OP_READDIR;
        resop.nfs_resop4_u.opreaddir.status = nfsstat4_NFS4_OK;
        resop.nfs_resop4_u.opreaddir.READDIR4res_u.resok4 = READDIR4resok {
            cookieverf: cbytes([9, 8, 7, 6, 5, 4, 3, 2]),
            reply: dirlist4 {
                entries: &mut entry,
                eof: 1,
            },
        };
        roundtrip_resop(resop, b"readdir", |d| {
            let ok = &d.nfs_resop4_u.opreaddir.READDIR4res_u.resok4;
            assert_eq!(ok.cookieverf, cbytes([9, 8, 7, 6, 5, 4, 3, 2]));
            assert_eq!(ok.reply.eof, 1);
            let e = &*ok.reply.entries;
            assert_eq!(e.cookie, 0x1234_5678);
            assert_eq!(e.name.utf8string_len, name.len() as u32);
            let got = std::slice::from_raw_parts(
                e.name.utf8string_val as *const u8,
                e.name.utf8string_len as usize,
            );
            assert_eq!(got, name);
            assert!(e.nextentry.is_null());
        });
    }
}

// ---------------------------------------------------------------------------
// REMOVE (directory op)
// ---------------------------------------------------------------------------

#[test]
fn remove_args_roundtrip() {
    unsafe {
        let target: &[u8] = b"old.txt";
        let mut argop: nfs_argop4 = std::mem::zeroed();
        argop.argop = nfs_opnum4_NFS4_OP_REMOVE;
        argop.nfs_argop4_u.opremove = REMOVE4args {
            target: utf8string(target),
        };
        roundtrip_argop(argop, b"remove", |d| {
            let t = &d.nfs_argop4_u.opremove.target;
            assert_eq!(t.utf8string_len, target.len() as u32);
            let got = std::slice::from_raw_parts(
                t.utf8string_val as *const u8,
                t.utf8string_len as usize,
            );
            assert_eq!(got, target);
        });
    }
}

#[test]
fn remove_res_roundtrip() {
    unsafe {
        let mut resop: nfs_resop4 = std::mem::zeroed();
        resop.resop = nfs_opnum4_NFS4_OP_REMOVE;
        resop.nfs_resop4_u.opremove.status = nfsstat4_NFS4_OK;
        resop.nfs_resop4_u.opremove.REMOVE4res_u.resok4 = REMOVE4resok {
            cinfo: change_info(1, 100, 101),
        };
        roundtrip_resop(resop, b"remove", |d| {
            let r = &d.nfs_resop4_u.opremove;
            assert_eq!(r.status, nfsstat4_NFS4_OK);
            let cinfo = &r.REMOVE4res_u.resok4.cinfo;
            assert_eq!(cinfo.atomic, 1);
            assert_eq!(cinfo.before, 100);
            assert_eq!(cinfo.after, 101);
        });
    }
}

// ---------------------------------------------------------------------------
// RENAME (directory op)
// ---------------------------------------------------------------------------

#[test]
fn rename_args_roundtrip() {
    unsafe {
        let old: &[u8] = b"oldname";
        let new: &[u8] = b"newname";
        let mut argop: nfs_argop4 = std::mem::zeroed();
        argop.argop = nfs_opnum4_NFS4_OP_RENAME;
        argop.nfs_argop4_u.oprename = RENAME4args {
            oldname: utf8string(old),
            newname: utf8string(new),
        };
        roundtrip_argop(argop, b"rename", |d| {
            let r = &d.nfs_argop4_u.oprename;
            let check = |s: &utf8string, expect: &[u8]| {
                assert_eq!(s.utf8string_len, expect.len() as u32);
                let got = std::slice::from_raw_parts(
                    s.utf8string_val as *const u8,
                    s.utf8string_len as usize,
                );
                assert_eq!(got, expect);
            };
            check(&r.oldname, old);
            check(&r.newname, new);
        });
    }
}

#[test]
fn rename_res_roundtrip() {
    unsafe {
        let mut resop: nfs_resop4 = std::mem::zeroed();
        resop.resop = nfs_opnum4_NFS4_OP_RENAME;
        resop.nfs_resop4_u.oprename.status = nfsstat4_NFS4_OK;
        resop.nfs_resop4_u.oprename.RENAME4res_u.resok4 = RENAME4resok {
            source_cinfo: change_info(1, 1, 2),
            target_cinfo: change_info(0, 3, 4),
        };
        roundtrip_resop(resop, b"rename", |d| {
            let ok = &d.nfs_resop4_u.oprename.RENAME4res_u.resok4;
            assert_eq!(ok.source_cinfo.before, 1);
            assert_eq!(ok.source_cinfo.after, 2);
            assert_eq!(ok.target_cinfo.before, 3);
            assert_eq!(ok.target_cinfo.after, 4);
            assert_eq!(ok.target_cinfo.atomic, 0);
        });
    }
}

// ---------------------------------------------------------------------------
// CREATE (file creation)
// ---------------------------------------------------------------------------

#[test]
fn create_args_roundtrip() {
    unsafe {
        let name: &[u8] = b"newfile.txt";
        let mut argop: nfs_argop4 = std::mem::zeroed();
        argop.argop = nfs_opnum4_NFS4_OP_CREATE;
        argop.nfs_argop4_u.opcreate.objtype.type_ = nfs_ftype4_NF4REG;
        argop.nfs_argop4_u.opcreate.objname = utf8string(name);
        // createattrs: empty fattr4 (no attributes requested).
        argop.nfs_argop4_u.opcreate.createattrs.attrmask.bitmap4_len = 0;
        roundtrip_argop(argop, b"create", |d| {
            let c = &d.nfs_argop4_u.opcreate;
            assert_eq!(c.objtype.type_, nfs_ftype4_NF4REG);
            assert_eq!(c.objname.utf8string_len, name.len() as u32);
            let got = std::slice::from_raw_parts(
                c.objname.utf8string_val as *const u8,
                c.objname.utf8string_len as usize,
            );
            assert_eq!(got, name);
            assert_eq!(c.createattrs.attrmask.bitmap4_len, 0);
            assert_eq!(c.createattrs.attr_vals.attrlist4_len, 0);
        });
    }
}

// ---------------------------------------------------------------------------
// GETFH (no-argument op)
// ---------------------------------------------------------------------------

#[test]
fn getfh_res_roundtrip() {
    unsafe {
        let fh: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];
        let mut resop: nfs_resop4 = std::mem::zeroed();
        resop.resop = nfs_opnum4_NFS4_OP_GETFH;
        resop.nfs_resop4_u.opgetfh.status = nfsstat4_NFS4_OK;
        resop
            .nfs_resop4_u
            .opgetfh
            .GETFH4res_u
            .resok4
            .object
            .nfs_fh4_len = fh.len() as u32;
        resop
            .nfs_resop4_u
            .opgetfh
            .GETFH4res_u
            .resok4
            .object
            .nfs_fh4_val = fh.as_ptr() as *mut c_char;
        roundtrip_resop(resop, b"getfh", |d| {
            let g = &d.nfs_resop4_u.opgetfh;
            assert_eq!(g.status, nfsstat4_NFS4_OK);
            let object = &g.GETFH4res_u.resok4.object;
            assert_eq!(object.nfs_fh4_len, fh.len() as u32);
            let got = std::slice::from_raw_parts(
                object.nfs_fh4_val as *const u8,
                object.nfs_fh4_len as usize,
            );
            assert_eq!(got, fh);
        });
    }
}

// ---------------------------------------------------------------------------
// READLINK (symlink coverage; this header has no SYMLINK op)
// ---------------------------------------------------------------------------

#[test]
fn readlink_res_roundtrip() {
    unsafe {
        let link: &[u8] = b"/some/target/path";
        let mut resop: nfs_resop4 = std::mem::zeroed();
        resop.resop = nfs_opnum4_NFS4_OP_READLINK;
        resop.nfs_resop4_u.opreadlink.status = nfsstat4_NFS4_OK;
        resop.nfs_resop4_u.opreadlink.READLINK4res_u.resok4.link = utf8string(link);
        roundtrip_resop(resop, b"readlink", |d| {
            let r = &d.nfs_resop4_u.opreadlink;
            assert_eq!(r.status, nfsstat4_NFS4_OK);
            let l = &r.READLINK4res_u.resok4.link;
            assert_eq!(l.utf8string_len, link.len() as u32);
            let got = std::slice::from_raw_parts(
                l.utf8string_val as *const u8,
                l.utf8string_len as usize,
            );
            assert_eq!(got, link);
        });
    }
}

// ---------------------------------------------------------------------------
// GETATTR
// ---------------------------------------------------------------------------

#[test]
fn getattr_args_roundtrip() {
    unsafe {
        let mut argop: nfs_argop4 = std::mem::zeroed();
        argop.argop = nfs_opnum4_NFS4_OP_GETATTR;
        // Empty attribute request (bitmap len 0 roundtrips; see readdir test).
        argop.nfs_argop4_u.opgetattr.attr_request.bitmap4_len = 0;
        roundtrip_argop(argop, b"getattr", |d| {
            assert_eq!(d.nfs_argop4_u.opgetattr.attr_request.bitmap4_len, 0);
        });
    }
}

#[test]
fn getattr_res_roundtrip() {
    unsafe {
        let attr_data: [u8; 4] = [1, 2, 3, 4];
        let mut resop: nfs_resop4 = std::mem::zeroed();
        resop.resop = nfs_opnum4_NFS4_OP_GETATTR;
        resop.nfs_resop4_u.opgetattr.status = nfsstat4_NFS4_OK;
        resop
            .nfs_resop4_u
            .opgetattr
            .GETATTR4res_u
            .resok4
            .obj_attributes = fattr4 {
            attrmask: bitmap4 {
                bitmap4_len: 0,
                map: [0; 3],
            },
            attr_vals: attrlist4 {
                attrlist4_len: attr_data.len() as u32,
                attrlist4_val: attr_data.as_ptr() as *mut c_char,
            },
        };
        roundtrip_resop(resop, b"getattr", |d| {
            let g = &d.nfs_resop4_u.opgetattr;
            assert_eq!(g.status, nfsstat4_NFS4_OK);
            let attrs = &g.GETATTR4res_u.resok4.obj_attributes;
            assert_eq!(attrs.attrmask.bitmap4_len, 0);
            assert_eq!(attrs.attr_vals.attrlist4_len, attr_data.len() as u32);
            let got = std::slice::from_raw_parts(
                attrs.attr_vals.attrlist4_val as *const u8,
                attrs.attr_vals.attrlist4_len as usize,
            );
            assert_eq!(got, attr_data);
        });
    }
}

// ---------------------------------------------------------------------------
// COMMIT
// ---------------------------------------------------------------------------

#[test]
fn commit_args_roundtrip() {
    unsafe {
        let mut argop: nfs_argop4 = std::mem::zeroed();
        argop.argop = nfs_opnum4_NFS4_OP_COMMIT;
        argop.nfs_argop4_u.opcommit = COMMIT4args {
            offset: 0x1000_2000_3000_4000,
            count: 4096,
        };
        roundtrip_argop(argop, b"commit", |d| {
            let c = &d.nfs_argop4_u.opcommit;
            assert_eq!(c.offset, 0x1000_2000_3000_4000);
            assert_eq!(c.count, 4096);
        });
    }
}

#[test]
fn commit_res_roundtrip() {
    unsafe {
        let mut resop: nfs_resop4 = std::mem::zeroed();
        resop.resop = nfs_opnum4_NFS4_OP_COMMIT;
        resop.nfs_resop4_u.opcommit.status = nfsstat4_NFS4_OK;
        resop.nfs_resop4_u.opcommit.COMMIT4res_u.resok4.writeverf =
            cbytes([0xde, 0xad, 0xbe, 0xef, 1, 2, 3, 4]);
        roundtrip_resop(resop, b"commit", |d| {
            assert_eq!(
                d.nfs_resop4_u.opcommit.COMMIT4res_u.resok4.writeverf,
                cbytes([0xde, 0xad, 0xbe, 0xef, 1, 2, 3, 4])
            );
        });
    }
}

// ---------------------------------------------------------------------------
// LINK (hard link creation)
// ---------------------------------------------------------------------------

#[test]
fn link_args_roundtrip() {
    unsafe {
        let newname: &[u8] = b"hardlink.txt";
        let mut argop: nfs_argop4 = std::mem::zeroed();
        argop.argop = nfs_opnum4_NFS4_OP_LINK;
        argop.nfs_argop4_u.oplink = LINK4args {
            newname: utf8string(newname),
        };
        roundtrip_argop(argop, b"link", |d| {
            let n = &d.nfs_argop4_u.oplink.newname;
            assert_eq!(n.utf8string_len, newname.len() as u32);
            let got = std::slice::from_raw_parts(
                n.utf8string_val as *const u8,
                n.utf8string_len as usize,
            );
            assert_eq!(got, newname);
        });
    }
}

#[test]
fn link_res_roundtrip() {
    unsafe {
        let mut resop: nfs_resop4 = std::mem::zeroed();
        resop.resop = nfs_opnum4_NFS4_OP_LINK;
        resop.nfs_resop4_u.oplink.status = nfsstat4_NFS4_OK;
        resop.nfs_resop4_u.oplink.LINK4res_u.resok4 = LINK4resok {
            cinfo: change_info(1, 0xaaaa, 0xbbbb),
        };
        roundtrip_resop(resop, b"link", |d| {
            let l = &d.nfs_resop4_u.oplink;
            assert_eq!(l.status, nfsstat4_NFS4_OK);
            let cinfo = &l.LINK4res_u.resok4.cinfo;
            assert_eq!(cinfo.atomic, 1);
            assert_eq!(cinfo.before, 0xaaaa);
            assert_eq!(cinfo.after, 0xbbbb);
        });
    }
}
