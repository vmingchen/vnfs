//! High-level NFSv4.1 operations on top of the session.

use std::os::raw::c_char;

use nfsv41_sys::*;

use crate::compound::{Compound, CompoundRes};
use crate::error::RpcResult;
use crate::session::Session;

/// An NFS file handle owned by the client.
#[derive(Clone, Debug)]
pub struct FileHandle {
    bytes: Vec<u8>,
}

impl FileHandle {
    fn as_nfs_fh(&self) -> nfs_fh4 {
        nfs_fh4 {
            nfs_fh4_len: self.bytes.len() as u32,
            nfs_fh4_val: self.bytes.as_ptr() as *mut c_char,
        }
    }

    fn from_nfs_fh(fh: &nfs_fh4) -> FileHandle {
        let slice = unsafe {
            std::slice::from_raw_parts(fh.nfs_fh4_val as *const u8, fh.nfs_fh4_len as usize)
        };
        FileHandle {
            bytes: slice.to_vec(),
        }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

pub struct NfsClient {
    session: Session,
    root: FileHandle,
}

/// How an OPEN handles a file that does not exist yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpenCreate {
    /// Never create the file; fail with NFS4ERR_NOENT if absent.
    NoCreate,
    /// Create with EXCLUSIVE4 semantics: fail with NFS4ERR_EXIST if present.
    Exclusive,
    /// Create with GUARDED4 semantics: create if absent, succeed if present.
    Guarded,
}

/// An entry returned by READDIR.
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub cookie: u64,
    /// Raw XDR-encoded attribute list, in the order requested.
    pub attrs: Vec<u8>,
}

/// A directory's listing (entries so far and the cookie to continue).
pub struct ChildListing {
    pub fh: FileHandle,
    pub entries: Vec<DirEntry>,
    pub cookie: u64,
}

/// One READ of a batched compound, `[PUTFH, READ]`.
pub struct ReadOp {
    pub fh: FileHandle,
    pub stateid: stateid4,
    pub offset: u64,
    pub count: u32,
}

/// One WRITE of a batched compound, `[PUTFH, WRITE]`.
pub struct WriteOp {
    pub fh: FileHandle,
    pub stateid: stateid4,
    pub offset: u64,
    pub data: Vec<u8>,
}

/// One GETATTR of a batched compound, `[PUTFH, GETATTR]`.
pub struct GetattrOp {
    pub fh: FileHandle,
    pub attrs: Vec<u32>,
}

/// One SETATTR of a batched compound, `[PUTFH, SETATTR]`.
pub struct SetattrOp {
    pub fh: FileHandle,
    pub mode: Option<u32>,
    pub size: Option<u64>,
}

/// One READLINK of a batched compound, `[PUTFH, READLINK]`.
pub struct ReadlinkOp {
    pub fh: FileHandle,
}

/// One RENAME of a batched compound: `[PUTFH src, SAVEFH, PUTFH dst, RENAME]`.
pub struct RenameOp {
    pub srcdir: FileHandle,
    pub oldname: String,
    pub dstdir: FileHandle,
    pub newname: String,
}

/// One CREATE of a batched compound, `[PUTFH dir, CREATE]` (mkdir / symlink).
pub struct CreateOp {
    pub dir: FileHandle,
    pub name: String,
    pub ftype: nfs_ftype4,
    pub linkdata: Option<Vec<u8>>,
}

/// One LINK of a batched compound: `[PUTFH src, SAVEFH, PUTFH dst, LINK]`.
pub struct LinkOp {
    pub dstdir: FileHandle,
    pub src: FileHandle,
    pub newname: String,
}

/// One OPEN of a batched compound, `[PUTFH dir, OPEN, GETFH]`.
pub struct OpenOp {
    pub dir: FileHandle,
    pub name: String,
    pub access: u32,
    pub create: OpenCreate,
}

/// One CLOSE of a batched compound, `[PUTFH fh, CLOSE]`.
pub struct CloseOp {
    pub fh: FileHandle,
    pub stateid: stateid4,
}

/// The server confirmed `ca_maxoperations` from CREATE_SESSION; keep every
/// compound (plus the implicit SEQUENCE) under it.
const MAX_COMPOUND_OPS: usize = 256;

/// FATTR4 attribute ids requested for every READDIR entry, in wire order.
/// Keep in sync with the parse order in `nfs.rs::parse_attrs`. Note:
/// FATTR4_TIME_CREATE is intentionally absent (ganesha omits it, and it maps
/// to creation time, not stat's ctime).
pub const READDIR_ATTRS: [u32; 11] = [
    FATTR4_TYPE,
    FATTR4_SIZE,
    FATTR4_FILEID,
    FATTR4_MODE,
    FATTR4_NUMLINKS,
    FATTR4_OWNER,
    FATTR4_OWNER_GROUP,
    FATTR4_RAWDEV,
    FATTR4_SPACE_USED,
    FATTR4_TIME_ACCESS,
    FATTR4_TIME_MODIFY,
];

impl NfsClient {
    /// Connect, run the session handshake, and resolve the export root.
    pub fn connect(host: &str) -> RpcResult<NfsClient> {
        let mut session = Session::connect(host)?;
        let root = session_mount_root(&mut session)?;
        Ok(NfsClient { session, root })
    }

    pub fn root(&self) -> &FileHandle {
        &self.root
    }

    /// Look up a single component below `dir`.
    pub fn lookup(&mut self, dir: &FileHandle, name: &str) -> RpcResult<FileHandle> {
        let mut c = Compound::new();
        c.tag(b"lookup");
        c.putfh(&dir.as_nfs_fh());
        c.lookup(name.as_bytes());
        c.getfh();
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(FileHandle::from_nfs_fh(res.getfh(3)))
    }

    /// Resolve a slash-separated path from the export root in a single
    /// compound: `[PUTFH root, LOOKUP a, LOOKUP b, ..., GETFH]`. After each
    /// LOOKUP the current filehandle is the looked-up object, so consecutive
    /// LOOKUPs chain without intermediate round trips.
    pub fn resolve(&mut self, path: &str) -> RpcResult<FileHandle> {
        let mut c = Compound::new();
        c.tag(b"resolve");
        c.putfh(&self.root.as_nfs_fh());
        let mut ncomps = 0usize;
        for comp in path.trim_matches('/').split('/') {
            if !comp.is_empty() {
                c.lookup(comp.as_bytes());
                ncomps += 1;
            }
        }
        if ncomps == 0 {
            return Ok(self.root.clone());
        }
        c.getfh();
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(FileHandle::from_nfs_fh(res.getfh(2 + ncomps)))
    }

    /// WRITE several `[PUTFH, WRITE]` pairs in as few compounds as possible.
    /// Returns `(bytes written, committed)` per op.
    ///
    /// Note: there is no batched READ counterpart. The kernel nfsd does not
    /// handle multiple READ ops per compound correctly (its reply-page offset
    /// computation collides sub-page reads), so reads are issued one per
    /// compound.
    /// READ several `[PUTFH, READ]` pairs in as few compounds as possible.
    /// Each i-th read in a compound is at resop `2 + 2*i` (SEQUENCE, PUTFH,
    /// READ, PUTFH, READ, ...). The kernel nfsd does not serve multiple READ
    /// ops per compound correctly; use an nfs-ganesha server for this.
    pub fn readv(&mut self, ops: &[ReadOp]) -> RpcResult<Vec<Vec<u8>>> {
        let mut out = Vec::with_capacity(ops.len());
        let per_chunk = (MAX_COMPOUND_OPS - 1) / 2;
        for chunk in ops.chunks(per_chunk) {
            let mut c = Compound::new();
            c.tag(b"readv");
            for op in chunk {
                c.putfh(&op.fh.as_nfs_fh());
                c.read(&op.stateid, op.offset, op.count);
            }
            let res = self.session.compound(&mut c)?;
            self.session.expect_all_ok(&res)?;
            for (i, _) in chunk.iter().enumerate() {
                let ok = res.read(2 + 2 * i);
                let len = ok.data.data_len as usize;
                let data = if len == 0 {
                    Vec::new()
                } else {
                    unsafe { std::slice::from_raw_parts(ok.data.data_val as *const u8, len) }
                        .to_vec()
                };
                out.push(data);
            }
        }
        Ok(out)
    }

    pub fn writev(&mut self, ops: &[WriteOp]) -> RpcResult<Vec<(u32, u32)>> {
        let mut out = Vec::with_capacity(ops.len());
        let per_chunk = (MAX_COMPOUND_OPS - 1) / 2;
        for chunk in ops.chunks(per_chunk) {
            let mut c = Compound::new();
            c.tag(b"writev");
            for op in chunk {
                c.putfh(&op.fh.as_nfs_fh());
                c.write(&op.stateid, op.offset, stable_how4_FILE_SYNC4, &op.data);
            }
            let res = self.session.compound(&mut c)?;
            self.session.expect_all_ok(&res)?;
            for (i, _) in chunk.iter().enumerate() {
                let ok = res.write(2 + 2 * i);
                out.push((ok.count, ok.committed));
            }
        }
        Ok(out)
    }

    /// REMOVE several names from `dir` in one compound. REMOVE leaves the
    /// current filehandle on `dir`, so consecutive REMOVEs chain.
    pub fn remove_many(&mut self, dir: &FileHandle, names: &[&str]) -> RpcResult<()> {
        let mut c = Compound::new();
        c.tag(b"removev");
        c.putfh(&dir.as_nfs_fh());
        for n in names {
            c.remove(n.as_bytes());
        }
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(())
    }

    /// OPEN a file below `dir`. `create` controls creation semantics. Returns
    /// the file handle and the open stateid.
    pub fn open(
        &mut self,
        dir: &FileHandle,
        name: &str,
        access: u32,
        create: OpenCreate,
    ) -> RpcResult<(FileHandle, stateid4)> {
        let mut c = Compound::new();
        c.tag(b"open");
        c.putfh(&dir.as_nfs_fh());
        let openhow = make_open_how(create, self.session.open_owner.verifier);
        c.open_claim_null(
            self.session.open_owner.seqid,
            access,
            OPEN4_SHARE_DENY_NONE,
            self.session.clientid,
            &self.session.open_owner.name,
            openhow,
            name.as_bytes(),
        );
        c.getfh();
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        let stateid = res.open(2).stateid;
        let fh = res.getfh(3);
        self.session.open_owner.seqid += 1;
        Ok((FileHandle::from_nfs_fh(fh), stateid))
    }

    /// Run a batch of same-shaped operations across as few compounds as
    /// possible. `add` appends `per_op` ops for each element to the current
    /// compound (with the element's global index, for seqid-based ops);
    /// `extract` reads the result of the i-th element from a compound reply.
    fn batch_ops<T, R>(
        &mut self,
        tag: &[u8],
        per_op: usize,
        ops: &[T],
        mut add: impl FnMut(&mut Compound, &T, usize),
        extract: impl Fn(&CompoundRes, usize) -> R,
    ) -> RpcResult<Vec<R>> {
        let chunk_size = (MAX_COMPOUND_OPS - 1) / per_op;
        let mut out = Vec::with_capacity(ops.len());
        let mut global = 0usize;
        for chunk in ops.chunks(chunk_size) {
            let mut c = Compound::new();
            c.tag(tag);
            for op in chunk {
                add(&mut c, op, global);
                global += 1;
            }
            let res = self.session.compound(&mut c)?;
            self.session.expect_all_ok(&res)?;
            for (i, _) in chunk.iter().enumerate() {
                out.push(extract(&res, i));
            }
        }
        Ok(out)
    }

    /// GETATTR several files in as few compounds as possible; returns the raw
    /// attribute list per file, in request order.
    pub fn getattr_many(&mut self, ops: &[GetattrOp]) -> RpcResult<Vec<Vec<u8>>> {
        self.batch_ops(
            b"getattrv",
            2,
            ops,
            |c, op, _| {
                c.putfh(&op.fh.as_nfs_fh());
                c.getattr(&op.attrs);
            },
            |res, i| res.getattr_bytes(2 + 2 * i),
        )
    }

    /// SETATTR mode and/or size on several files in one compound.
    pub fn setattr_many(&mut self, ops: &[SetattrOp]) -> RpcResult<()> {
        let _ = self.batch_ops::<SetattrOp, ()>(
            b"setattrv",
            2,
            ops,
            |c, op, _| {
                c.putfh(&op.fh.as_nfs_fh());
                c.setattr(op.mode, op.size);
            },
            |_, _| (),
        )?;
        Ok(())
    }

    /// READLINK several files in as few compounds as possible.
    pub fn readlink_many(&mut self, ops: &[ReadlinkOp]) -> RpcResult<Vec<Vec<u8>>> {
        self.batch_ops(
            b"readlinkv",
            2,
            ops,
            |c, op, _| {
                c.putfh(&op.fh.as_nfs_fh());
                c.readlink();
            },
            |res, i| res.readlink(2 + 2 * i).to_vec(),
        )
    }

    /// RENAME several pairs in as few compounds as possible. Each pair is
    /// `[PUTFH src, SAVEFH, PUTFH dst, RENAME]`.
    pub fn rename_many(&mut self, ops: &[RenameOp]) -> RpcResult<()> {
        let _ = self.batch_ops::<RenameOp, ()>(
            b"renamev",
            4,
            ops,
            |c, op, _| {
                c.putfh(&op.srcdir.as_nfs_fh());
                c.savefh();
                c.putfh(&op.dstdir.as_nfs_fh());
                c.rename(op.oldname.as_bytes(), op.newname.as_bytes());
            },
            |_, _| (),
        )?;
        Ok(())
    }

    /// CREATE several objects (mkdir / symlink) in as few compounds as
    /// possible. CREATE changes the current filehandle, so each gets its own
    /// `[PUTFH dir, CREATE]`.
    pub fn create_many(&mut self, ops: &[CreateOp]) -> RpcResult<()> {
        let _ = self.batch_ops::<CreateOp, ()>(
            b"createv",
            2,
            ops,
            |c, op, _| {
                c.putfh(&op.dir.as_nfs_fh());
                c.create(op.name.as_bytes(), op.ftype, op.linkdata.as_deref());
            },
            |_, _| (),
        )?;
        Ok(())
    }

    /// LINK several sources into their destinations in as few compounds as
    /// possible. Each is `[PUTFH src, SAVEFH, PUTFH dst, LINK]`.
    pub fn link_many(&mut self, ops: &[LinkOp]) -> RpcResult<()> {
        let _ = self.batch_ops::<LinkOp, ()>(
            b"linkv",
            4,
            ops,
            |c, op, _| {
                c.putfh(&op.src.as_nfs_fh());
                c.savefh();
                c.putfh(&op.dstdir.as_nfs_fh());
                c.link(op.newname.as_bytes());
            },
            |_, _| (),
        )?;
        Ok(())
    }

    /// OPEN several files in as few compounds as possible; each is
    /// `[PUTFH dir, OPEN, GETFH]`. Open-owner seqids are assigned
    /// consecutively across the batch.
    pub fn open_many(&mut self, ops: &[OpenOp]) -> RpcResult<Vec<(FileHandle, stateid4)>> {
        let base = self.session.open_owner.seqid;
        let verifier = self.session.open_owner.verifier;
        let clientid = self.session.clientid;
        let owner_name = self.session.open_owner.name.clone();
        let n = ops.len();
        let out = self.batch_ops(
            b"openv",
            3,
            ops,
            |c, op, gi| {
                c.putfh(&op.dir.as_nfs_fh());
                let openhow = make_open_how(op.create, verifier);
                c.open_claim_null(
                    base + gi as u32,
                    op.access,
                    OPEN4_SHARE_DENY_NONE,
                    clientid,
                    &owner_name,
                    openhow,
                    op.name.as_bytes(),
                );
                c.getfh();
            },
            |res, i| {
                let stateid = res.open(2 + 3 * i).stateid;
                let fh = res.getfh(3 + 3 * i);
                (FileHandle::from_nfs_fh(fh), stateid)
            },
        )?;
        self.session.open_owner.seqid = base + n as u32;
        Ok(out)
    }

    /// CLOSE several files in as few compounds as possible. Close seqids are
    /// assigned consecutively across the batch.
    pub fn close_many(&mut self, ops: &[CloseOp]) -> RpcResult<()> {
        let base = self.session.open_owner.seqid;
        let n = ops.len();
        let _ = self.batch_ops::<CloseOp, ()>(
            b"closev",
            2,
            ops,
            |c, op, gi| {
                c.putfh(&op.fh.as_nfs_fh());
                c.close(base + gi as u32, &op.stateid);
            },
            |_, _| (),
        )?;
        self.session.open_owner.seqid = base + n as u32;
        Ok(())
    }

    /// READ `count` bytes at `offset`; returns the data read.
    pub fn read(
        &mut self,
        fh: &FileHandle,
        stateid: &stateid4,
        offset: u64,
        count: u32,
    ) -> RpcResult<Vec<u8>> {
        let mut c = Compound::new();
        c.tag(b"read");
        c.putfh(&fh.as_nfs_fh());
        c.read(stateid, offset, count);
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        let ok = res.read(2);
        let len = ok.data.data_len as usize;
        if len == 0 {
            return Ok(Vec::new());
        }
        let data = unsafe { std::slice::from_raw_parts(ok.data.data_val as *const u8, len) };
        Ok(data.to_vec())
    }

    /// WRITE `data` at `offset` with FILE_SYNC stability; returns bytes
    /// written and committed.
    pub fn write(
        &mut self,
        fh: &FileHandle,
        stateid: &stateid4,
        offset: u64,
        data: &[u8],
    ) -> RpcResult<(u32, u32)> {
        let mut c = Compound::new();
        c.tag(b"write");
        c.putfh(&fh.as_nfs_fh());
        c.write(stateid, offset, stable_how4_FILE_SYNC4, data);
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        let ok = res.write(2);
        Ok((ok.count, ok.committed))
    }

    /// CLOSE the open file.
    pub fn close(&mut self, fh: &FileHandle, stateid: &stateid4) -> RpcResult<()> {
        let mut c = Compound::new();
        c.tag(b"close");
        c.putfh(&fh.as_nfs_fh());
        c.close(self.session.open_owner.seqid, stateid);
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        self.session.open_owner.seqid += 1;
        Ok(())
    }

    /// CREATE a new object below `dir`. `ftype` is the object type (NF4DIR
    /// for mkdir, NF4LNK for a symlink with `linkdata`). Returns the handle
    /// of the new object.
    fn create(
        &mut self,
        dir: &FileHandle,
        name: &str,
        ftype: nfs_ftype4,
        linkdata: Option<&str>,
    ) -> RpcResult<FileHandle> {
        let mut c = Compound::new();
        c.tag(b"create");
        c.putfh(&dir.as_nfs_fh());
        c.create(name.as_bytes(), ftype, linkdata.map(|s| s.as_bytes()));
        c.getfh();
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(FileHandle::from_nfs_fh(res.getfh(3)))
    }

    /// Create a directory `name` below `dir`.
    pub fn mkdir(&mut self, dir: &FileHandle, name: &str) -> RpcResult<FileHandle> {
        self.create(dir, name, nfs_ftype4_NF4DIR, None)
    }

    /// Create a symbolic link `name` below `dir` pointing at `target`.
    pub fn symlink(&mut self, dir: &FileHandle, name: &str, target: &str) -> RpcResult<FileHandle> {
        self.create(dir, name, nfs_ftype4_NF4LNK, Some(target))
    }

    /// Read the target of the symlink at `fh`.
    pub fn readlink(&mut self, fh: &FileHandle) -> RpcResult<Vec<u8>> {
        let mut c = Compound::new();
        c.tag(b"readlink");
        c.putfh(&fh.as_nfs_fh());
        c.readlink();
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(res.readlink(2).to_vec())
    }

    /// GETATTR the requested FATTR4 attributes of `fh`; returns the raw XDR
    /// attribute list in request order.
    pub fn getattr(&mut self, fh: &FileHandle, attrs: &[u32]) -> RpcResult<Vec<u8>> {
        let mut c = Compound::new();
        c.tag(b"getattr");
        c.putfh(&fh.as_nfs_fh());
        c.getattr(attrs);
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(res.getattr_bytes(2))
    }

    /// SETATTR mode and/or size on `fh`.
    pub fn setattr(
        &mut self,
        fh: &FileHandle,
        mode: Option<u32>,
        size: Option<u64>,
    ) -> RpcResult<()> {
        let mut c = Compound::new();
        c.tag(b"setattr");
        c.putfh(&fh.as_nfs_fh());
        c.setattr(mode, size);
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(())
    }

    /// READDIR `dir` starting at `cookie`; returns entries with names, next
    /// cookies, and requested attributes. Skips "." and "..".
    pub fn readdir(&mut self, dir: &FileHandle, cookie: u64) -> RpcResult<Vec<DirEntry>> {
        let mut c = Compound::new();
        c.tag(b"readdir");
        c.putfh(&dir.as_nfs_fh());
        let zeroverf: verifier4 = [0; 8];
        c.readdir(cookie, &zeroverf, 256 * 1024, 1024 * 1024, &READDIR_ATTRS);
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(Self::collect_readdir(res.readdir(2)).0)
    }

    /// Extract the entries and the next cookie from a decoded READDIR reply.
    fn collect_readdir(ok: &READDIR4resok) -> (Vec<DirEntry>, u64) {
        let mut out = Vec::new();
        let mut cookie = 0u64;
        let mut e = ok.reply.entries;
        while !e.is_null() {
            let ent = unsafe { &*e };
            let name_len = ent.name.utf8string_len as usize;
            let name = if name_len == 0 {
                String::new()
            } else {
                let name = unsafe {
                    std::slice::from_raw_parts(ent.name.utf8string_val as *const u8, name_len)
                };
                String::from_utf8_lossy(name).into_owned()
            };
            if name != "." && name != ".." {
                let attrs_len = ent.attrs.attr_vals.attrlist4_len as usize;
                let attrs = if attrs_len == 0 {
                    Vec::new()
                } else {
                    unsafe {
                        std::slice::from_raw_parts(
                            ent.attrs.attr_vals.attrlist4_val as *const u8,
                            attrs_len,
                        )
                    }
                    .to_vec()
                };
                out.push(DirEntry {
                    name,
                    cookie: ent.cookie,
                    attrs,
                });
            }
            cookie = ent.cookie;
            e = ent.nextentry;
        }
        (out, cookie)
    }

    /// For each `(parent, child_name)` pair, LOOKUP the child, GETFH its
    /// handle, and READDIR its first page -- all in as few compounds as
    /// possible (`[PUTFH parent, LOOKUP, GETFH, READDIR]` per child).
    pub fn readdir_children(
        &mut self,
        ops: &[(FileHandle, String)],
    ) -> RpcResult<Vec<ChildListing>> {
        let per_chunk = (MAX_COMPOUND_OPS - 1) / 4;
        let mut out = Vec::with_capacity(ops.len());
        let zeroverf: verifier4 = [0; 8];
        for chunk in ops.chunks(per_chunk) {
            let mut c = Compound::new();
            c.tag(b"readdir_children");
            for (pfh, name) in chunk {
                c.putfh(&pfh.as_nfs_fh());
                c.lookup(name.as_bytes());
                c.getfh();
                c.readdir(0, &zeroverf, 256 * 1024, 1024 * 1024, &READDIR_ATTRS);
            }
            let res = self.session.compound(&mut c)?;
            self.session.expect_all_ok(&res)?;
            for (i, _) in chunk.iter().enumerate() {
                let fh = res.getfh(3 + 4 * i);
                let (entries, cookie) = Self::collect_readdir(res.readdir(4 + 4 * i));
                out.push(ChildListing {
                    fh: FileHandle::from_nfs_fh(fh),
                    entries,
                    cookie,
                });
            }
        }
        Ok(out)
    }

    /// For each `(fh, cookie)`, continue READDIR with the next page in as few
    /// compounds as possible (`[PUTFH fh, READDIR(cookie)]` per dir).
    pub fn readdir_pages(
        &mut self,
        ops: &[(FileHandle, u64)],
    ) -> RpcResult<Vec<(Vec<DirEntry>, u64)>> {
        let per_chunk = (MAX_COMPOUND_OPS - 1) / 2;
        let mut out = Vec::with_capacity(ops.len());
        let zeroverf: verifier4 = [0; 8];
        for chunk in ops.chunks(per_chunk) {
            let mut c = Compound::new();
            c.tag(b"readdir_pages");
            for (fh, cookie) in chunk {
                c.putfh(&fh.as_nfs_fh());
                c.readdir(*cookie, &zeroverf, 256 * 1024, 1024 * 1024, &READDIR_ATTRS);
            }
            let res = self.session.compound(&mut c)?;
            self.session.expect_all_ok(&res)?;
            for (i, _) in chunk.iter().enumerate() {
                let (entries, cookie) = Self::collect_readdir(res.readdir(2 + 2 * i));
                out.push((entries, cookie));
            }
        }
        Ok(out)
    }

    /// REMOVE `name` from directory `dir`.
    pub fn remove(&mut self, dir: &FileHandle, name: &str) -> RpcResult<()> {
        let mut c = Compound::new();
        c.tag(b"remove");
        c.putfh(&dir.as_nfs_fh());
        c.remove(name.as_bytes());
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(())
    }

    /// RENAME `oldname` out of `srcdir` to `newname` in `dstdir`. The kernel
    /// reads the source directory from the saved filehandle, so we PUTFH the
    /// source dir, SAVEFH it, then PUTFH the target dir before the RENAME op.
    pub fn rename(
        &mut self,
        srcdir: &FileHandle,
        oldname: &str,
        dstdir: &FileHandle,
        newname: &str,
    ) -> RpcResult<()> {
        let mut c = Compound::new();
        c.tag(b"rename");
        c.putfh(&srcdir.as_nfs_fh());
        c.savefh();
        c.putfh(&dstdir.as_nfs_fh());
        c.rename(oldname.as_bytes(), newname.as_bytes());
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(())
    }

    /// Create a hard link named `newname` in directory `dir` to `src`.
    /// Requires the saved-fh trick: SAVEFH the source, then LINK into `dir`.
    pub fn link(&mut self, dir: &FileHandle, src: &FileHandle, newname: &str) -> RpcResult<()> {
        let mut c = Compound::new();
        c.tag(b"link");
        c.putfh(&src.as_nfs_fh());
        c.savefh();
        c.putfh(&dir.as_nfs_fh());
        c.link(newname.as_bytes());
        let res = self.session.compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(())
    }
}

fn session_mount_root(session: &mut Session) -> RpcResult<FileHandle> {
    let mut c = Compound::new();
    c.tag(b"mount");
    c.putrootfh();
    c.getfh();
    let res = session.compound(&mut c)?;
    session.expect_all_ok(&res)?;
    Ok(FileHandle::from_nfs_fh(res.getfh(2)))
}

/// Build the `openflag4` for an OPEN based on the create mode.
fn make_open_how(create: OpenCreate, verifier: verifier4) -> openflag4 {
    match create {
        OpenCreate::NoCreate => openflag4 {
            opentype: opentype4_OPEN4_NOCREATE,
            openflag4_u: openflag4__bindgen_ty_1 {
                how: unsafe { std::mem::zeroed() },
            },
        },
        OpenCreate::Exclusive => openflag4 {
            opentype: opentype4_OPEN4_CREATE,
            openflag4_u: openflag4__bindgen_ty_1 {
                how: createhow4 {
                    mode: createmode4_EXCLUSIVE4,
                    createhow4_u: createhow4__bindgen_ty_1 {
                        createverf: verifier,
                    },
                },
            },
        },
        OpenCreate::Guarded => openflag4 {
            opentype: opentype4_OPEN4_CREATE,
            openflag4_u: openflag4__bindgen_ty_1 {
                how: createhow4 {
                    mode: createmode4_GUARDED4,
                    createhow4_u: createhow4__bindgen_ty_1 {
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
                    },
                },
            },
        },
    }
}
