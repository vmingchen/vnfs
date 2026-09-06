//! COMPOUND4 request building and reply handling.

// bindgen emits lowercase constants (e.g. nfs_opnum4_NFS4_OP_READ) that we
// must match against; silence the style lint for those patterns.
#![allow(non_upper_case_globals)]

use std::os::raw::{c_char, c_void};

use nfsv41_sys::*;

use crate::error::RpcResult;
use crate::rpc::{NFSPROC4_COMPOUND, RpcClient};

unsafe extern "C" fn wrap_compound4args(xdrs: *mut libntirpc_sys::XDR, objp: *mut c_void) -> bool {
    unsafe { xdr_wrap_COMPOUND4args(xdrs as *mut nfsv41_sys::XDR, objp as *mut COMPOUND4args) }
}

unsafe extern "C" fn wrap_compound4res(xdrs: *mut libntirpc_sys::XDR, objp: *mut c_void) -> bool {
    unsafe { xdr_wrap_COMPOUND4res(xdrs as *mut nfsv41_sys::XDR, objp as *mut COMPOUND4res) }
}

/// A COMPOUND4args under construction. Owns backing buffers for every
/// variable-length field (names, owner ids, data) referenced by the ops.
pub struct Compound {
    pub args: COMPOUND4args,
    ops: Vec<nfs_argop4>,
    keep: Vec<Vec<u8>>,
}

impl Default for Compound {
    fn default() -> Self {
        Self::new()
    }
}

impl Compound {
    pub fn new() -> Compound {
        Compound {
            args: COMPOUND4args {
                tag: utf8string {
                    utf8string_len: 0,
                    utf8string_val: std::ptr::null_mut(),
                },
                minorversion: 1,
                argarray: COMPOUND4args__bindgen_ty_1 {
                    argarray_len: 0,
                    argarray_val: std::ptr::null_mut(),
                },
            },
            ops: Vec::new(),
            keep: Vec::new(),
        }
    }

    fn push(&mut self, op: nfs_argop4) {
        self.ops.push(op);
    }

    fn insert0(&mut self, op: nfs_argop4) {
        self.ops.insert(0, op);
    }

    /// Number of operations currently in the request (before Session adds
    /// the mandatory SEQUENCE operation).
    pub(crate) fn op_count(&self) -> usize {
        self.ops.len()
    }

    fn keep(&mut self, bytes: &[u8]) -> (*mut c_char, u32) {
        let buf = bytes.to_vec();
        let ptr = buf.as_ptr() as *mut c_char;
        let len = buf.len() as u32;
        self.keep.push(buf);
        (ptr, len)
    }

    /// Build a bitmap4 whose bit `a` is set for every FATTR4 attribute id in
    /// `attrs` (ids must be in increasing order for the wire format).
    fn bitmap(attrs: &[u32]) -> bitmap4 {
        let mut map = [0u32; 3];
        for &a in attrs {
            let word = (a / 32) as usize;
            let bit = a % 32;
            map[word] |= 1 << bit;
        }
        let mut len = 0;
        for (i, &w) in map.iter().enumerate() {
            if w != 0 {
                len = (i + 1) as u32;
            }
        }
        bitmap4 {
            bitmap4_len: len,
            map,
        }
    }

    pub fn tag(&mut self, tag: &[u8]) {
        let (ptr, len) = self.keep(tag);
        self.args.tag = utf8string {
            utf8string_len: len,
            utf8string_val: ptr,
        };
    }

    pub fn putfh(&mut self, fh: &nfs_fh4) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_PUTFH;
        op.nfs_argop4_u.opputfh = PUTFH4args { object: *fh };
        self.push(op);
    }

    pub fn putrootfh(&mut self) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_PUTROOTFH;
        self.push(op);
    }

    pub fn getfh(&mut self) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_GETFH;
        self.push(op);
    }

    pub fn lookup(&mut self, name: &[u8]) {
        let (ptr, len) = self.keep(name);
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_LOOKUP;
        op.nfs_argop4_u.oplookup = LOOKUP4args {
            objname: utf8string {
                utf8string_len: len,
                utf8string_val: ptr,
            },
        };
        self.push(op);
    }

    pub fn sequence(
        &mut self,
        sessionid: &sessionid4,
        seqid: u32,
        slotid: u32,
        highest_slotid: u32,
    ) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_SEQUENCE;
        op.nfs_argop4_u.opsequence = SEQUENCE4args {
            sa_sessionid: *sessionid,
            sa_sequenceid: seqid,
            sa_slotid: slotid,
            sa_highest_slotid: highest_slotid,
            sa_cachethis: 0,
        };
        self.push(op);
    }

    pub fn open(&mut self, args: OPEN4args) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_OPEN;
        op.nfs_argop4_u.opopen = args;
        self.push(op);
    }

    /// Build a CLAIM_NULL OPEN with the given owner and filename.
    #[allow(clippy::too_many_arguments)]
    pub fn open_claim_null(
        &mut self,
        seqid: u32,
        share_access: u32,
        share_deny: u32,
        clientid: clientid4,
        owner_name: &[u8],
        openhow: openflag4,
        claim_file: &[u8],
    ) {
        let (own_ptr, own_len) = self.keep(owner_name);
        let (file_ptr, file_len) = self.keep(claim_file);
        self.open(OPEN4args {
            seqid,
            share_access,
            share_deny,
            owner: state_owner4 {
                clientid,
                owner: state_owner4__bindgen_ty_1 {
                    owner_len: own_len,
                    owner_val: own_ptr,
                },
            },
            openhow,
            claim: open_claim4 {
                claim: open_claim_type4_CLAIM_NULL,
                open_claim4_u: open_claim4__bindgen_ty_1 {
                    file: utf8string {
                        utf8string_len: file_len,
                        utf8string_val: file_ptr,
                    },
                },
            },
        });
    }

    /// Like [`open_claim_null`](Self::open_claim_null) but with an
    /// `OPEN4_CREATE`/`UNCHECKED4` openhow carrying a creation mode and/or a
    /// `size=0` truncation in its createattrs. The mode is applied by the
    /// server only when the file is created; `size=0` also truncates an
    /// existing file (the NFSv4.1 equivalent of `O_TRUNC`, RFC 8881
    /// §18.16.3).
    #[allow(clippy::too_many_arguments)]
    pub fn open_claim_null_create_mode(
        &mut self,
        seqid: u32,
        share_access: u32,
        share_deny: u32,
        clientid: clientid4,
        owner_name: &[u8],
        claim_file: &[u8],
        mode: Option<u32>,
        truncate: bool,
    ) {
        let mut map = [0u32; 3];
        let mut vals = Vec::with_capacity(12);
        // Attribute values follow the bitmap in increasing attribute-id order:
        // FATTR4_SIZE (word 0) before FATTR4_MODE (word 1).
        if truncate {
            map[0] |= 1 << (FATTR4_SIZE % 32);
            vals.extend_from_slice(&0u64.to_be_bytes());
        }
        if let Some(mode) = mode {
            map[1] |= 1 << (FATTR4_MODE % 32);
            vals.extend_from_slice(&mode.to_be_bytes());
        }
        let bitmap_len = if map[2] != 0 {
            3
        } else if map[1] != 0 {
            2
        } else if map[0] != 0 {
            1
        } else {
            0
        };
        let (vptr, vlen) = self.keep(&vals);
        let openhow = openflag4 {
            opentype: opentype4_OPEN4_CREATE,
            openflag4_u: openflag4__bindgen_ty_1 {
                how: createhow4 {
                    mode: createmode4_UNCHECKED4,
                    createhow4_u: createhow4__bindgen_ty_1 {
                        createattrs: fattr4 {
                            attrmask: bitmap4 {
                                bitmap4_len: bitmap_len,
                                map,
                            },
                            attr_vals: attrlist4 {
                                attrlist4_len: vlen,
                                attrlist4_val: vptr,
                            },
                        },
                    },
                },
            },
        };
        self.open_claim_null(
            seqid,
            share_access,
            share_deny,
            clientid,
            owner_name,
            openhow,
            claim_file,
        );
    }

    pub fn read(&mut self, stateid: &stateid4, offset: u64, count: u32) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_READ;
        op.nfs_argop4_u.opread = READ4args {
            stateid: *stateid,
            offset,
            count,
        };
        self.push(op);
    }

    pub fn write(&mut self, stateid: &stateid4, offset: u64, stable: u32, data: &[u8]) {
        let (ptr, len) = self.keep(data);
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_WRITE;
        op.nfs_argop4_u.opwrite = WRITE4args {
            stateid: *stateid,
            offset,
            stable,
            data: WRITE4args__bindgen_ty_1 {
                data_len: len,
                data_val: ptr,
            },
        };
        self.push(op);
    }

    /// NFSv4.2 COPY. The saved filehandle is the source and the current
    /// filehandle is the destination.
    pub fn copy(
        &mut self,
        src_stateid: &stateid4,
        dst_stateid: &stateid4,
        src_offset: u64,
        dst_offset: u64,
        count: u64,
    ) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_COPY;
        op.nfs_argop4_u.opcopy = COPY4args {
            ca_src_stateid: *src_stateid,
            ca_dst_stateid: *dst_stateid,
            ca_src_offset: src_offset,
            ca_dst_offset: dst_offset,
            ca_count: count,
            ca_consecutive: 1,
            ca_synchronous: 1,
            ca_source_server_len: 0,
            ca_source_server_val: std::ptr::null_mut(),
        };
        self.push(op);
    }

    pub fn close(&mut self, seqid: u32, stateid: &stateid4) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_CLOSE;
        op.nfs_argop4_u.opclose = CLOSE4args {
            seqid,
            open_stateid: *stateid,
        };
        self.push(op);
    }

    pub fn exchange_id(&mut self, args: EXCHANGE_ID4args) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_EXCHANGE_ID;
        op.nfs_argop4_u.opexchange_id = args;
        self.push(op);
    }

    pub fn create_session(&mut self, args: CREATE_SESSION4args) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_CREATE_SESSION;
        op.nfs_argop4_u.opcreate_session = args;
        self.push(op);
    }

    pub fn reclaim_complete(&mut self) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_RECLAIM_COMPLETE;
        op.nfs_argop4_u.opreclaim_complete = RECLAIM_COMPLETE4args { rca_one_fs: 0 };
        self.push(op);
    }

    /// CREATE a new object below the current directory. `ftype` is the object
    /// type (e.g. NF4DIR for mkdir, NF4LNK for a symlink); when it is NF4LNK,
    /// `linkdata` is the symlink target. This is the single NFSv4 op that
    /// implements the high-level mkdir / symlink calls.
    pub fn create(&mut self, name: &[u8], ftype: nfs_ftype4, linkdata: Option<&[u8]>) {
        let (nptr, nlen) = self.keep(name);
        let objtype = match linkdata {
            Some(target) => {
                let (tptr, tlen) = self.keep(target);
                createtype4 {
                    type_: nfs_ftype4_NF4LNK,
                    createtype4_u: createtype4__bindgen_ty_1 {
                        linkdata: utf8string {
                            utf8string_len: tlen,
                            utf8string_val: tptr,
                        },
                    },
                }
            }
            None => createtype4 {
                type_: ftype,
                createtype4_u: unsafe { std::mem::zeroed() },
            },
        };
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_CREATE;
        op.nfs_argop4_u.opcreate = CREATE4args {
            objtype,
            objname: utf8string {
                utf8string_len: nlen,
                utf8string_val: nptr,
            },
            createattrs: fattr4 {
                attrmask: bitmap4 {
                    bitmap4_len: 0,
                    map: [0; 3],
                },
                attr_vals: attrlist4 {
                    attrlist4_len: 0,
                    attrlist4_val: std::ptr::null_mut(),
                },
            },
        };
        self.push(op);
    }

    /// READLINK the current object (no arguments).
    pub fn readlink(&mut self) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_READLINK;
        self.push(op);
    }

    /// GETATTR the current object for the given FATTR4 attribute ids.
    pub fn getattr(&mut self, attrs: &[u32]) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_GETATTR;
        op.nfs_argop4_u.opgetattr = GETATTR4args {
            attr_request: Self::bitmap(attrs),
        };
        self.push(op);
    }

    /// SETATTR mode and/or size on the current object. A None value leaves the
    /// corresponding attribute unchanged. Uses the zero stateid (current
    /// state).
    pub fn setattr(&mut self, mode: Option<u32>, size: Option<u64>) {
        self.setattr_with_stateid(
            mode,
            size,
            &stateid4 {
                seqid: 0,
                other: [0; 12],
            },
        );
    }

    /// Like [`setattr`](Self::setattr) but with an explicit stateid. In a
    /// compound that already OPENed the file, pass the special "current"
    /// stateid (seqid 1, other zeros) so the server resolves it to the open
    /// state without invalidating the compound's current-stateid tracking
    /// (the all-0 stateid marks it invalid, breaking a later special-stateid
    /// CLOSE on Ganesha).
    pub fn setattr_with_stateid(
        &mut self,
        mode: Option<u32>,
        size: Option<u64>,
        stateid: &stateid4,
    ) {
        let mut map = [0u32; 3];
        let mut vals: Vec<u8> = Vec::new();
        if let Some(m) = mode {
            map[1] |= 1 << (FATTR4_MODE % 32);
            vals.extend_from_slice(&m.to_be_bytes());
        }
        if let Some(s) = size {
            map[0] |= 1 << (FATTR4_SIZE % 32);
            vals.extend_from_slice(&s.to_be_bytes());
        }
        let mut bitmap_len = 0;
        for (i, &w) in map.iter().enumerate() {
            if w != 0 {
                bitmap_len = (i + 1) as u32;
            }
        }
        let (vptr, vlen) = self.keep(&vals);
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_SETATTR;
        op.nfs_argop4_u.opsetattr = SETATTR4args {
            stateid: *stateid,
            obj_attributes: fattr4 {
                attrmask: bitmap4 {
                    bitmap4_len: bitmap_len,
                    map,
                },
                attr_vals: attrlist4 {
                    attrlist4_len: vlen,
                    attrlist4_val: vptr,
                },
            },
        };
        self.push(op);
    }

    /// READDIR the current directory from `cookie`; `cookieverf` guards the
    /// cookie, `attrs` are the FATTR4 ids requested for each entry.
    pub fn readdir(
        &mut self,
        cookie: u64,
        cookieverf: &verifier4,
        dircount: u32,
        maxcount: u32,
        attrs: &[u32],
    ) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_READDIR;
        op.nfs_argop4_u.opreaddir = READDIR4args {
            cookie,
            cookieverf: *cookieverf,
            dircount,
            maxcount,
            attr_request: Self::bitmap(attrs),
        };
        self.push(op);
    }

    /// REMOVE `name` from the current directory.
    pub fn remove(&mut self, name: &[u8]) {
        let (ptr, len) = self.keep(name);
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_REMOVE;
        op.nfs_argop4_u.opremove = REMOVE4args {
            target: utf8string {
                utf8string_len: len,
                utf8string_val: ptr,
            },
        };
        self.push(op);
    }

    /// RENAME `oldname` to `newname` within the current directory.
    pub fn rename(&mut self, oldname: &[u8], newname: &[u8]) {
        let (optr, olen) = self.keep(oldname);
        let (nptr, nlen) = self.keep(newname);
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_RENAME;
        op.nfs_argop4_u.oprename = RENAME4args {
            oldname: utf8string {
                utf8string_len: olen,
                utf8string_val: optr,
            },
            newname: utf8string {
                utf8string_len: nlen,
                utf8string_val: nptr,
            },
        };
        self.push(op);
    }

    /// SAVEFH the current filehandle.
    pub fn savefh(&mut self) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_SAVEFH;
        self.push(op);
    }

    /// RESTOREFH: make the saved filehandle the current filehandle. In
    /// NFSv4.1 this is how a compound returns to a directory it previously
    /// SAVEFH'd, enabling many child operations in one compound.
    pub fn restorefh(&mut self) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_RESTOREFH;
        self.push(op);
    }

    /// LOOKUPP: make the parent of the current filehandle the current
    /// filehandle.
    pub fn lookupp(&mut self) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_LOOKUPP;
        self.push(op);
    }

    /// LINK the saved filehandle into the current directory under `newname`.
    pub fn link(&mut self, newname: &[u8]) {
        let (ptr, len) = self.keep(newname);
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_LINK;
        op.nfs_argop4_u.oplink = LINK4args {
            newname: utf8string {
                utf8string_len: len,
                utf8string_val: ptr,
            },
        };
        self.push(op);
    }

    pub fn destroy_session(&mut self, sessionid: &sessionid4) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_DESTROY_SESSION;
        op.nfs_argop4_u.opdestroy_session = DESTROY_SESSION4args {
            dsa_sessionid: *sessionid,
        };
        self.push(op);
    }

    pub fn destroy_clientid(&mut self, clientid: clientid4) {
        let mut op: nfs_argop4 = unsafe { std::mem::zeroed() };
        op.argop = nfs_opnum4_NFS4_OP_DESTROY_CLIENTID;
        op.nfs_argop4_u.opdestroy_clientid = DESTROY_CLIENTID4args {
            dca_clientid: clientid,
        };
        self.push(op);
    }

    /// Encode and send the compound; returns the decoded reply.
    pub fn call(&mut self, rpc: &RpcClient) -> RpcResult<CompoundRes> {
        self.args.argarray.argarray_len = self.ops.len() as u_int;
        self.args.argarray.argarray_val = self.ops.as_mut_ptr();
        let mut res: COMPOUND4res = unsafe { std::mem::zeroed() };
        let t0 = std::time::Instant::now();
        rpc.call(
            NFSPROC4_COMPOUND,
            Some(wrap_compound4args),
            &mut self.args as *mut _ as *mut c_void,
            Some(wrap_compound4res),
            &mut res as *mut _ as *mut c_void,
        )?;
        RPC_TIME_US.fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
        RPC_CALLS.fetch_add(1, Ordering::Relaxed);
        compound_stats_record(&self.args);
        Ok(CompoundRes { res })
    }

    /// Access for Session to prepend a SEQUENCE op.
    pub fn prepend_sequence(&mut self, op: nfs_argop4) {
        self.insert0(op);
    }
}

/// A decoded COMPOUND4res; frees its XDR-allocated storage on drop.
pub struct CompoundRes {
    pub res: COMPOUND4res,
}

impl Drop for CompoundRes {
    fn drop(&mut self) {
        // The XDR-free idiom requires a reference to the shared null stream.
        #[allow(static_mut_refs)]
        unsafe {
            xdr_wrap_COMPOUND4res(&mut xdr_free_null_stream, &mut self.res)
        };
    }
}

impl CompoundRes {
    pub fn status(&self) -> u32 {
        self.res.status
    }

    pub fn nops(&self) -> usize {
        self.res.resarray.resarray_len as usize
    }

    fn resop(&self, i: usize) -> &nfs_resop4 {
        assert!(i < self.nops(), "resop index out of range");
        unsafe { self.res.resarray.resarray_val.add(i).as_ref().unwrap() }
    }

    /// Per-op status for the given resop index.
    pub fn op_status(&self, i: usize) -> u32 {
        let ro = self.resop(i);
        unsafe {
            match ro.resop {
                nfs_opnum4_NFS4_OP_PUTFH => ro.nfs_resop4_u.opputfh.status,
                nfs_opnum4_NFS4_OP_PUTROOTFH => ro.nfs_resop4_u.opputrootfh.status,
                nfs_opnum4_NFS4_OP_GETFH => ro.nfs_resop4_u.opgetfh.status,
                nfs_opnum4_NFS4_OP_LOOKUP => ro.nfs_resop4_u.oplookup.status,
                nfs_opnum4_NFS4_OP_SEQUENCE => ro.nfs_resop4_u.opsequence.sr_status,
                nfs_opnum4_NFS4_OP_OPEN => ro.nfs_resop4_u.opopen.status,
                nfs_opnum4_NFS4_OP_READ => ro.nfs_resop4_u.opread.status,
                nfs_opnum4_NFS4_OP_WRITE => ro.nfs_resop4_u.opwrite.status,
                nfs_opnum4_NFS4_OP_COPY => ro.nfs_resop4_u.opcopy.cr_status,
                nfs_opnum4_NFS4_OP_CLOSE => ro.nfs_resop4_u.opclose.status,
                nfs_opnum4_NFS4_OP_EXCHANGE_ID => ro.nfs_resop4_u.opexchange_id.eir_status,
                nfs_opnum4_NFS4_OP_CREATE_SESSION => ro.nfs_resop4_u.opcreate_session.csr_status,
                nfs_opnum4_NFS4_OP_RECLAIM_COMPLETE => {
                    ro.nfs_resop4_u.opreclaim_complete.rcr_status
                }
                nfs_opnum4_NFS4_OP_CREATE => ro.nfs_resop4_u.opcreate.status,
                nfs_opnum4_NFS4_OP_READLINK => ro.nfs_resop4_u.opreadlink.status,
                nfs_opnum4_NFS4_OP_GETATTR => ro.nfs_resop4_u.opgetattr.status,
                nfs_opnum4_NFS4_OP_SETATTR => ro.nfs_resop4_u.opsetattr.status,
                nfs_opnum4_NFS4_OP_READDIR => ro.nfs_resop4_u.opreaddir.status,
                nfs_opnum4_NFS4_OP_REMOVE => ro.nfs_resop4_u.opremove.status,
                nfs_opnum4_NFS4_OP_RENAME => ro.nfs_resop4_u.oprename.status,
                nfs_opnum4_NFS4_OP_LINK => ro.nfs_resop4_u.oplink.status,
                nfs_opnum4_NFS4_OP_SAVEFH => ro.nfs_resop4_u.opsavefh.status,
                nfs_opnum4_NFS4_OP_RESTOREFH => ro.nfs_resop4_u.oprestorefh.status,
                nfs_opnum4_NFS4_OP_LOOKUPP => ro.nfs_resop4_u.oplookupp.status,
                nfs_opnum4_NFS4_OP_DESTROY_SESSION => ro.nfs_resop4_u.opdestroy_session.dsr_status,
                nfs_opnum4_NFS4_OP_DESTROY_CLIENTID => {
                    ro.nfs_resop4_u.opdestroy_clientid.dcr_status
                }
                _ => u32::MAX,
            }
        }
    }

    pub fn op(&self, i: usize) -> &nfs_resop4 {
        self.resop(i)
    }

    /// Return the resop at `i`, asserting it is the expected op. Turns a
    /// wrong-op union access into a clear panic instead of reading garbage.
    fn expect_op(&self, i: usize, want: nfs_opnum4, what: &str) -> &nfs_resop4 {
        let ro = self.resop(i);
        assert_eq!(
            ro.resop, want,
            "op {} is not {} (got {})",
            i, what, ro.resop
        );
        ro
    }

    /// The `EXCHANGE_ID4resok` of resop `i`.
    pub fn exchange_id(&self, i: usize) -> &EXCHANGE_ID4resok {
        let ro = self.expect_op(i, nfs_opnum4_NFS4_OP_EXCHANGE_ID, "EXCHANGE_ID");
        unsafe { &ro.nfs_resop4_u.opexchange_id.EXCHANGE_ID4res_u.eir_resok4 }
    }

    /// The `CREATE_SESSION4resok` of resop `i`.
    pub fn create_session(&self, i: usize) -> &CREATE_SESSION4resok {
        let ro = self.expect_op(i, nfs_opnum4_NFS4_OP_CREATE_SESSION, "CREATE_SESSION");
        unsafe {
            &ro.nfs_resop4_u
                .opcreate_session
                .CREATE_SESSION4res_u
                .csr_resok4
        }
    }

    /// The file handle returned by the GETFH at resop `i`.
    pub fn getfh(&self, i: usize) -> &nfs_fh4 {
        let ro = self.expect_op(i, nfs_opnum4_NFS4_OP_GETFH, "GETFH");
        unsafe { &ro.nfs_resop4_u.opgetfh.GETFH4res_u.resok4.object }
    }

    /// The `OPEN4resok` of resop `i`.
    pub fn open(&self, i: usize) -> &OPEN4resok {
        let ro = self.expect_op(i, nfs_opnum4_NFS4_OP_OPEN, "OPEN");
        unsafe { &ro.nfs_resop4_u.opopen.OPEN4res_u.resok4 }
    }

    /// The `READ4resok` of resop `i`.
    pub fn read(&self, i: usize) -> &READ4resok {
        let ro = self.expect_op(i, nfs_opnum4_NFS4_OP_READ, "READ");
        unsafe { &ro.nfs_resop4_u.opread.READ4res_u.resok4 }
    }

    /// The `WRITE4resok` of resop `i`.
    pub fn write(&self, i: usize) -> &WRITE4resok {
        let ro = self.expect_op(i, nfs_opnum4_NFS4_OP_WRITE, "WRITE");
        unsafe { &ro.nfs_resop4_u.opwrite.WRITE4res_u.resok4 }
    }

    /// The successful NFSv4.2 COPY result at resop `i`.
    pub fn copy(&self, i: usize) -> &COPY4resok {
        let ro = self.expect_op(i, nfs_opnum4_NFS4_OP_COPY, "COPY");
        unsafe { &ro.nfs_resop4_u.opcopy.COPY4res_u.cr_resok4 }
    }

    /// The symlink target bytes of the READLINK at resop `i`.
    pub fn readlink(&self, i: usize) -> &[u8] {
        let ro = self.expect_op(i, nfs_opnum4_NFS4_OP_READLINK, "READLINK");
        let link = unsafe { ro.nfs_resop4_u.opreadlink.READLINK4res_u.resok4.link };
        let len = link.utf8string_len as usize;
        if len == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(link.utf8string_val as *const u8, len) }
    }

    /// The raw attribute list returned by the GETATTR at resop `i`.
    pub fn getattr(&self, i: usize) -> &[u8] {
        let ro = self.expect_op(i, nfs_opnum4_NFS4_OP_GETATTR, "GETATTR");
        let ok = unsafe { ro.nfs_resop4_u.opgetattr.GETATTR4res_u.resok4 };
        let len = ok.obj_attributes.attr_vals.attrlist4_len as usize;
        if len == 0 {
            return &[];
        }
        unsafe {
            std::slice::from_raw_parts(ok.obj_attributes.attr_vals.attrlist4_val as *const u8, len)
        }
    }

    /// The `READDIR4resok` of resop `i`.
    pub fn readdir(&self, i: usize) -> &READDIR4resok {
        let ro = self.expect_op(i, nfs_opnum4_NFS4_OP_READDIR, "READDIR");
        unsafe { &ro.nfs_resop4_u.opreaddir.READDIR4res_u.resok4 }
    }

    /// Return an owned copy of the raw attrlist bytes for `i` (`getattr`).
    pub fn getattr_bytes(&self, i: usize) -> Vec<u8> {
        self.getattr(i).to_vec()
    }
}

// ---------------------------------------------------------------------------
// Compound statistics (diagnostics)
// ---------------------------------------------------------------------------

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

thread_local! {
    /// Reused only when VNFS_STATS requests exact encoded byte counts.
    static STATS_XDR_BUFFER: std::cell::RefCell<Vec<u8>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

/// Counters for the compounds sent: total count, total operations (including
/// the implicit SEQUENCE), and total encoded request bytes.
pub static COMPOUND_COUNT: AtomicU64 = AtomicU64::new(0);
pub static COMPOUND_OPS: AtomicU64 = AtomicU64::new(0);
pub static COMPOUND_BYTES: AtomicU64 = AtomicU64::new(0);
pub static COMPOUND_MAX_OPS: AtomicU64 = AtomicU64::new(0);
pub static RPC_CALLS: AtomicU64 = AtomicU64::new(0);
pub static RPC_TIME_US: AtomicU64 = AtomicU64::new(0);

fn compound_stats_record(args: &COMPOUND4args) {
    let ops = args.argarray.argarray_len as u64;
    COMPOUND_COUNT.fetch_add(1, Ordering::Relaxed);
    COMPOUND_OPS.fetch_add(ops, Ordering::Relaxed);
    COMPOUND_MAX_OPS.fetch_max(ops, Ordering::Relaxed);
    static DUMP_ENABLED: OnceLock<bool> = OnceLock::new();
    let dump_enabled =
        *DUMP_ENABLED.get_or_init(|| std::env::var("VNFS_DUMP").as_deref() == Ok("1"));
    if dump_enabled && ops > 100 {
        let mut buf = String::new();
        let n = args.argarray.argarray_val;
        for i in 0..args.argarray.argarray_len as usize {
            use std::fmt::Write;
            let op = unsafe { (*n.add(i)).argop };
            if i > 0 {
                buf.push(' ');
            }
            let _ = write!(buf, "{}", op);
        }
        eprintln!("[dump] compound ops={}: {}", ops, buf);
    }
    // Exact size accounting requires a second XDR encode. Keep that expensive
    // diagnostic out of the normal I/O path; VNFS_STATS is an opt-in process
    // setting and must be present before the first compound is sent.
    static BYTE_STATS_ENABLED: OnceLock<bool> = OnceLock::new();
    let byte_stats_enabled =
        *BYTE_STATS_ENABLED.get_or_init(|| std::env::var("VNFS_STATS").as_deref() == Ok("1"));
    if !byte_stats_enabled {
        return;
    }
    STATS_XDR_BUFFER.with(|scratch| {
        let mut buf = scratch.borrow_mut();
        if buf.is_empty() {
            buf.resize(4 * 1024 * 1024, 0);
        }
        loop {
            let mut xdr: XDR = unsafe { std::mem::zeroed() };
            let encoded = unsafe {
                xdrmem_ncreate(
                    &mut xdr,
                    buf.as_mut_ptr() as *mut c_char,
                    buf.len() as u32,
                    xdr_op_XDR_ENCODE,
                );
                xdr_wrap_COMPOUND4args(&mut xdr, args as *const _ as *mut _)
            };
            if encoded {
                let len = unsafe { xdr.x_data.offset_from(xdr.x_v.vio_base) as u64 };
                COMPOUND_BYTES.fetch_add(len, Ordering::Relaxed);
                break;
            }
            // A diagnostic must not grow without bound if malformed input
            // reaches the encoder. This is well above the configured request
            // limit and only allocates when byte statistics are enabled.
            if buf.len() >= 256 * 1024 * 1024 {
                break;
            }
            let new_len = buf.len() * 2;
            buf.resize(new_len, 0);
        }
    });
}

/// Aggregate compound statistics, resetting the counters.
pub fn compound_stats() -> (u64, u64, u64, u64) {
    (
        COMPOUND_COUNT.swap(0, Ordering::Relaxed),
        COMPOUND_OPS.swap(0, Ordering::Relaxed),
        COMPOUND_BYTES.swap(0, Ordering::Relaxed),
        COMPOUND_MAX_OPS.swap(0, Ordering::Relaxed),
    )
}

/// Aggregate RPC round-trip timing, resetting the counters.
pub fn rpc_stats() -> (u64, u64) {
    (
        RPC_CALLS.swap(0, Ordering::Relaxed),
        RPC_TIME_US.swap(0, Ordering::Relaxed),
    )
}
