//! NFSv4.1 session setup: EXCHANGE_ID, CREATE_SESSION, RECLAIM_COMPLETE,
//! and per-compound SEQUENCE/seqid management.

use std::os::raw::c_char;
use std::time::{SystemTime, UNIX_EPOCH};

use nfsv41_sys::*;

use crate::compound::{Compound, CompoundRes};
use crate::error::{RpcError, RpcResult};
use crate::rpc::RpcClient;

/// An OPEN owner. seqid is incremented by the server after each OPEN.
pub struct OpenOwner {
    pub name: Vec<u8>,
    pub seqid: u32,
    pub verifier: verifier4,
}

pub struct Session {
    pub rpc: RpcClient,
    pub clientid: clientid4,
    pub sessionid: sessionid4,
    pub minorversion: u32,
    slot_seqid: u32,
    /// Set after an RPC error where it is unknowable whether the server
    /// consumed the slot sequence. A fresh session is required before reuse.
    poisoned: bool,
    /// Server-confirmed channel attributes from CREATE_SESSION: the
    /// negotiated maxima for compound request size and operation count.
    pub max_requestsize: usize,
    /// Maximum size of a compound reply (bounds total READ data per
    /// compound, since READ payloads travel in the reply).
    pub max_responsesize: usize,
    pub max_operations: usize,
    /// The open owner for user-visible descriptors (`open_by_path` /
    /// `openv`).
    pub open_owner: OpenOwner,
    /// A separate open owner for implicit opens made by path-based
    /// operations, so closing an internal open never revokes a stateid the
    /// caller still holds (kernel nfsd reuses one stateid per owner+file).
    pub path_owner: OpenOwner,
}

impl Session {
    pub fn connect(host: &str) -> RpcResult<Session> {
        Self::connect_minor(host, 1)
    }

    pub fn connect_minor(host: &str, minorversion: u32) -> RpcResult<Session> {
        let rpc = RpcClient::connect(host)?;
        let mut s = Session {
            rpc,
            clientid: 0,
            sessionid: [0; 16],
            minorversion,
            // New sessions are created with slot seqid 0, and the kernel's
            // check_slot_seqid() accepts seqid == slot_seqid + 1, so the
            // first SEQUENCE must carry seqid 1.
            slot_seqid: 1,
            poisoned: false,
            max_requestsize: 4 * 1024 * 1024,
            max_responsesize: 4 * 1024 * 1024,
            max_operations: 256,
            open_owner: OpenOwner {
                name: b"vnfs-open-owner".to_vec(),
                seqid: 0,
                verifier: make_verifier(),
            },
            path_owner: OpenOwner {
                name: b"vnfs-path-open-owner".to_vec(),
                seqid: 0,
                verifier: make_verifier(),
            },
        };
        s.exchange_id()?;
        s.create_session()?;
        s.reclaim_complete()?;
        Ok(s)
    }

    fn exchange_id(&mut self) -> RpcResult<()> {
        let verifier = make_verifier();
        // A unique owner id per connection so concurrent clients (parallel
        // tests, multiple processes) don't collide on the server's client
        // table and replace each other's confirmed clients mid-flight.
        let owner_id = format!(
            "vnfs-client-{}-{:x}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let owner_id = owner_id.as_bytes();
        let mut c = Compound::new();
        c.args.minorversion = self.minorversion;
        c.tag(b"exchange_id");
        c.exchange_id(EXCHANGE_ID4args {
            eia_clientowner: client_owner4 {
                co_verifier: verifier,
                co_ownerid: client_owner4__bindgen_ty_1 {
                    co_ownerid_len: owner_id.len() as u32,
                    co_ownerid_val: owner_id.as_ptr() as *mut c_char,
                },
            },
            eia_flags: 0,
            eia_state_protect: state_protect4_a {
                spa_how: state_protect_how4_SP4_NONE,
                state_protect4_a_u: state_protect4_a__bindgen_ty_1 {
                    spa_mach_ops: unsafe { std::mem::zeroed() },
                },
            },
            eia_client_impl_id: EXCHANGE_ID4args__bindgen_ty_1 {
                eia_client_impl_id_len: 0,
                eia_client_impl_id_val: std::ptr::null_mut(),
            },
        });
        let res = c.call(&self.rpc)?;
        let st = res.op_status(0);
        if st != nfsstat4_NFS4_OK {
            return Err(RpcError::op(0, st));
        }
        self.clientid = res.exchange_id(0).eir_clientid;
        Ok(())
    }

    fn create_session(&mut self) -> RpcResult<()> {
        let fore = channel_attrs4 {
            ca_headerpadsize: 0,
            ca_maxrequestsize: 4 * 1024 * 1024,
            ca_maxresponsesize: 4 * 1024 * 1024,
            ca_maxresponsesize_cached: 4 * 1024 * 1024,
            ca_maxoperations: 256,
            ca_maxrequests: 256,
            ca_rdma_ird: channel_attrs4__bindgen_ty_1 {
                ca_rdma_ird_len: 0,
                ca_rdma_ird_val: std::ptr::null_mut(),
            },
        };
        let back = channel_attrs4 {
            ca_headerpadsize: 0,
            ca_maxrequestsize: 4 * 1024 * 1024,
            ca_maxresponsesize: 4 * 1024 * 1024,
            ca_maxresponsesize_cached: 4 * 1024 * 1024,
            ca_maxoperations: 2,
            ca_maxrequests: 2,
            ca_rdma_ird: channel_attrs4__bindgen_ty_1 {
                ca_rdma_ird_len: 0,
                ca_rdma_ird_val: std::ptr::null_mut(),
            },
        };
        let mut c = Compound::new();
        c.args.minorversion = self.minorversion;
        c.tag(b"create_session");
        c.create_session(CREATE_SESSION4args {
            csa_clientid: self.clientid,
            csa_sequence: 1,
            csa_flags: 0,
            csa_fore_chan_attrs: fore,
            csa_back_chan_attrs: back,
            csa_cb_program: 0,
            csa_sec_parms: CREATE_SESSION4args__bindgen_ty_1 {
                csa_sec_parms_len: 0,
                csa_sec_parms_val: std::ptr::null_mut(),
            },
        });
        let res = c.call(&self.rpc)?;
        let st = res.op_status(0);
        if st != nfsstat4_NFS4_OK {
            return Err(RpcError::op(0, st));
        }
        let ok = res.create_session(0);
        self.sessionid = ok.csr_sessionid;
        // The server returns its confirmed channel attributes; compounds must
        // stay under these (the client's advertised values are only a
        // request). Zero is invalid and must not silently restore a larger
        // client-side default.
        let fore = &ok.csr_fore_chan_attrs;
        if fore.ca_maxrequestsize == 0 || fore.ca_maxresponsesize == 0 || fore.ca_maxoperations == 0
        {
            return Err(RpcError::transport(
                "server returned invalid zero-valued fore-channel limits",
            ));
        }
        self.max_requestsize = fore.ca_maxrequestsize as usize;
        self.max_responsesize = fore.ca_maxresponsesize as usize;
        self.max_operations = fore.ca_maxoperations as usize;
        Ok(())
    }

    fn reclaim_complete(&mut self) -> RpcResult<()> {
        let mut c = Compound::new();
        c.tag(b"reclaim_complete");
        c.reclaim_complete();
        // RECLAIM_COMPLETE requires a session: it must follow a SEQUENCE op.
        let res = self.compound(&mut c)?;
        let st = res.op_status(1);
        if st != nfsstat4_NFS4_OK {
            return Err(RpcError::op(1, st));
        }
        Ok(())
    }

    /// Prepend a SEQUENCE op and send the compound. The slot seqid advances
    /// whenever the server consumed the SEQUENCE (i.e. it returned NFS4_OK).
    pub fn compound(&mut self, c: &mut Compound) -> RpcResult<CompoundRes> {
        if self.poisoned {
            return Err(RpcError::transport(
                "NFS session is unusable after an ambiguous transport failure; reconnect",
            ));
        }
        c.args.minorversion = self.minorversion;
        let mut seq: nfs_argop4 = unsafe { std::mem::zeroed() };
        seq.argop = nfs_opnum4_NFS4_OP_SEQUENCE;
        seq.nfs_argop4_u.opsequence = SEQUENCE4args {
            sa_sessionid: self.sessionid,
            sa_sequenceid: self.slot_seqid,
            sa_slotid: 0,
            sa_highest_slotid: 0,
            sa_cachethis: 0,
        };
        c.prepend_sequence(seq);
        let res = match c.call(&self.rpc) {
            Ok(res) => res,
            Err(e) => {
                // The request may or may not have reached the server. Advancing
                // guesses wrong when it did not; reusing the sequence with a
                // new RPC XID guesses wrong when it did. Require reconnect
                // rather than silently corrupting the slot sequence.
                self.poisoned = true;
                return Err(e);
            }
        };
        // A server may reject an oversized compound before executing
        // SEQUENCE, returning a valid compound result with no per-op results.
        if res.nops() > 0 && res.op_status(0) == nfsstat4_NFS4_OK {
            self.slot_seqid += 1;
        }
        Ok(res)
    }

    /// Require the compound and every op to have succeeded, returning the
    /// index and NFS status of the first failure.
    pub fn expect_all_ok(&self, res: &CompoundRes) -> RpcResult<()> {
        if res.status() != nfsstat4_NFS4_OK {
            return Err(RpcError::op(0, res.status()));
        }
        for i in 0..res.nops() {
            let st = res.op_status(i);
            if st != nfsstat4_NFS4_OK {
                return Err(RpcError::op(i, st));
            }
        }
        Ok(())
    }

    /// Tear down the session and clientid on the server so a later run with a
    /// different credential doesn't trip NFS4ERR_CLID_INUSE (RFC 5661 case 3).
    /// Best-effort: never fails the caller.
    fn destroy(&mut self) {
        if self.clientid == 0 {
            return;
        }
        let mut c = Compound::new();
        c.args.minorversion = self.minorversion;
        c.tag(b"destroy_session");
        c.destroy_session(&self.sessionid);
        if let Ok(res) = c.call(&self.rpc) {
            let _ = res;
        }
        let mut c = Compound::new();
        c.args.minorversion = self.minorversion;
        c.tag(b"destroy_clientid");
        c.destroy_clientid(self.clientid);
        if let Ok(res) = c.call(&self.rpc) {
            let _ = res;
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// 8-byte opaque verifier derived from the current time.
pub fn make_verifier() -> verifier4 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mut v: verifier4 = [0; 8];
    for (i, b) in v.iter_mut().enumerate() {
        *b = ((now >> (8 * i)) & 0xff) as libc::c_char;
    }
    v
}
