//! Narrow, safe adapters from untrusted bytes to the NFS wire parsers.
//!
//! This module is intentionally available only with the `fuzzing` feature.
//! It owns every allocation referenced by a generated C structure, so the
//! fuzz target can vary counts and null pointers without manufacturing an
//! out-of-bounds pointer before the production validator gets control.

#![allow(non_upper_case_globals)]

use std::os::raw::c_char;

use nfsv41_sys::*;

use crate::compound::validate_response_ops;
use crate::nfs::validate_attr_list;

const OPS: [nfs_opnum4; 6] = [
    nfs_opnum4_NFS4_OP_PUTROOTFH,
    nfs_opnum4_NFS4_OP_GETFH,
    nfs_opnum4_NFS4_OP_READ,
    nfs_opnum4_NFS4_OP_READLINK,
    nfs_opnum4_NFS4_OP_GETATTR,
    u32::MAX,
];

const ATTRS: [u32; 14] = [
    FATTR4_TYPE,
    FATTR4_CHANGE,
    FATTR4_SIZE,
    FATTR4_NAMED_ATTR,
    FATTR4_FILEID,
    FATTR4_MODE,
    FATTR4_NUMLINKS,
    FATTR4_OWNER,
    FATTR4_OWNER_GROUP,
    FATTR4_RAWDEV,
    FATTR4_SPACE_USED,
    FATTR4_TIME_ACCESS,
    FATTR4_TIME_METADATA,
    FATTR4_TIME_MODIFY,
];

struct Bytes<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn u8(&mut self) -> u8 {
        let value = self.bytes.get(self.at).copied().unwrap_or(0);
        self.at = self.at.saturating_add(1);
        value
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }
}

fn operation(selector: u8) -> nfs_opnum4 {
    OPS[usize::from(selector) % OPS.len()]
}

fn argument(op: nfs_opnum4) -> nfs_argop4 {
    let mut value: nfs_argop4 = unsafe { std::mem::zeroed() };
    value.argop = op;
    value
}

fn response(
    op: nfs_opnum4,
    status: nfsstat4,
    payload_len: u32,
    payload: *mut c_char,
) -> nfs_resop4 {
    let mut value: nfs_resop4 = unsafe { std::mem::zeroed() };
    value.resop = op;
    match op {
        nfs_opnum4_NFS4_OP_PUTROOTFH => {
            value.nfs_resop4_u.opputrootfh.status = status;
        }
        nfs_opnum4_NFS4_OP_GETFH => {
            value.nfs_resop4_u.opgetfh.status = status;
            if status == nfsstat4_NFS4_OK {
                value.nfs_resop4_u.opgetfh.GETFH4res_u.resok4.object = nfs_fh4 {
                    nfs_fh4_len: payload_len,
                    nfs_fh4_val: payload,
                };
            }
        }
        nfs_opnum4_NFS4_OP_READ => {
            value.nfs_resop4_u.opread.status = status;
            if status == nfsstat4_NFS4_OK {
                value.nfs_resop4_u.opread.READ4res_u.resok4.data.data_len = payload_len;
                value.nfs_resop4_u.opread.READ4res_u.resok4.data.data_val = payload;
            }
        }
        nfs_opnum4_NFS4_OP_READLINK => {
            value.nfs_resop4_u.opreadlink.status = status;
            if status == nfsstat4_NFS4_OK {
                value
                    .nfs_resop4_u
                    .opreadlink
                    .READLINK4res_u
                    .resok4
                    .link
                    .utf8string_len = payload_len;
                value
                    .nfs_resop4_u
                    .opreadlink
                    .READLINK4res_u
                    .resok4
                    .link
                    .utf8string_val = payload;
            }
        }
        nfs_opnum4_NFS4_OP_GETATTR => {
            value.nfs_resop4_u.opgetattr.status = status;
            if status == nfsstat4_NFS4_OK {
                value
                    .nfs_resop4_u
                    .opgetattr
                    .GETATTR4res_u
                    .resok4
                    .obj_attributes
                    .attr_vals
                    .attrlist4_len = payload_len;
                value
                    .nfs_resop4_u
                    .opgetattr
                    .GETATTR4res_u
                    .resok4
                    .obj_attributes
                    .attr_vals
                    .attrlist4_val = payload;
            }
        }
        _ => {}
    }
    value
}

/// Exercise COMPOUND reply cardinality, opcode, status, and pointer checks.
pub fn compound_response(data: &[u8]) {
    let mut input = Bytes::new(data);
    let request_len = usize::from(input.u8() % 33);
    let declared_count = if input.u8() & 1 == 0 {
        u32::from(input.u8() % 34)
    } else {
        input.u32()
    };
    let null_array = input.u8() & 1 != 0;
    let compound_status = input.u32();

    let request: Vec<_> = (0..request_len)
        .map(|_| argument(operation(input.u8())))
        .collect();

    // If the count is larger than the request, the production validator must
    // reject it before looking through the array pointer. Otherwise allocate
    // exactly the declared number of elements so every non-null pointer is
    // valid independently of the generated contents.
    let allocated = usize::try_from(declared_count)
        .ok()
        .filter(|count| *count <= request_len)
        .unwrap_or(0);
    let mut payload = [0u8; 1];
    let mut responses = Vec::with_capacity(allocated);
    for _ in 0..allocated {
        let op = operation(input.u8());
        let status = match input.u8() % 4 {
            0 => nfsstat4_NFS4_OK,
            1 => compound_status,
            2 => nfsstat4_NFS4ERR_IO,
            _ => input.u32(),
        };
        let payload_len = input.u32();
        let payload_ptr = if input.u8() & 1 == 0 {
            payload.as_mut_ptr().cast::<c_char>()
        } else {
            std::ptr::null_mut()
        };
        responses.push(response(op, status, payload_len, payload_ptr));
    }

    let array = if null_array || responses.is_empty() {
        std::ptr::null_mut()
    } else {
        responses.as_mut_ptr()
    };
    let reply = COMPOUND4res {
        status: compound_status,
        tag: utf8string {
            utf8string_len: 0,
            utf8string_val: std::ptr::null_mut(),
        },
        resarray: COMPOUND4res__bindgen_ty_1 {
            resarray_len: declared_count,
            resarray_val: array,
        },
    };
    let _ = validate_response_ops(&request, &reply);
}

/// Exercise positional GETATTR decoding, including XDR string padding.
pub fn attribute_list(data: &[u8]) {
    let Some((&count, rest)) = data.split_first() else {
        let _ = validate_attr_list(&[], &[]);
        return;
    };
    let count = usize::from(count % 33);
    let id_bytes = rest.get(..count).unwrap_or(rest);
    let ids: Vec<_> = id_bytes
        .iter()
        .map(|selector| ATTRS[usize::from(*selector) % ATTRS.len()])
        .collect();
    let list = rest.get(id_bytes.len()..).unwrap_or_default();
    let _ = validate_attr_list(&ids, list);
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn arbitrary_compound_shapes_do_not_escape_validation(
            data in proptest::collection::vec(any::<u8>(), 0..512)
        ) {
            compound_response(&data);
        }

        #[test]
        fn arbitrary_attribute_lists_do_not_panic(
            data in proptest::collection::vec(any::<u8>(), 0..512)
        ) {
            attribute_list(&data);
        }
    }
}
