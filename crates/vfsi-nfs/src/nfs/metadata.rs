//! NFS attribute decoding and metadata operations.

use super::*;

// ---------------------------------------------------------------------------
// NFS attribute parsing
// ---------------------------------------------------------------------------

/// Parsed values of a GETATTR reply for the supported FATTR4 attributes.
#[derive(Debug, Clone, Default)]
pub(super) struct AttrValues {
    pub(super) ftype: Option<u32>,
    // Only successfully decoded/mapped attributes are present. TYPE is kept
    // separately because it has no AttrMask bit and is fetched independently.
    present: AttrMask,
    change: u64,
    mode: u32,
    size: u64,
    nlink: u32,
    fs_id: u128,
    fileid: u64,
    uid: u32,
    gid: u32,
    rdev: u64,
    blocks: u64,
    // Separate components avoid the padding of three (i64, u32) tuples.
    mtime_sec: i64,
    mtime_nsec: u32,
    atime_sec: i64,
    atime_nsec: u32,
    ctime_sec: i64,
    ctime_nsec: u32,
    has_named_attr: bool,
}

/// The full set of supported FATTR4 ids, in wire (increasing) order. Must
/// match `crate::client::READDIR_ATTRS`.
const FULL_ATTR_IDS: [u32; 15] = [
    FATTR4_TYPE,
    FATTR4_CHANGE,
    FATTR4_SIZE,
    FATTR4_NAMED_ATTR,
    FATTR4_FSID,
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

pub(super) fn request_mask_to_attr_list(masks: &AttrMask) -> Vec<u32> {
    let mut ids = Vec::new();
    for id in FULL_ATTR_IDS {
        let wanted = match id {
            FATTR4_TYPE => true, // always fetch type (cheap, aids listdir)
            FATTR4_CHANGE => masks.contains(AttrMask::CHANGE),
            FATTR4_SIZE => masks.contains(AttrMask::SIZE),
            FATTR4_FSID => masks.contains(AttrMask::FS_ID),
            FATTR4_NAMED_ATTR => masks.contains(AttrMask::NAMED_ATTR),
            FATTR4_FILEID => masks.contains(AttrMask::FILEID),
            FATTR4_MODE => masks.contains(AttrMask::MODE),
            FATTR4_NUMLINKS => masks.contains(AttrMask::NLINK),
            FATTR4_OWNER => masks.contains(AttrMask::UID),
            FATTR4_OWNER_GROUP => masks.contains(AttrMask::GID),
            FATTR4_RAWDEV => masks.contains(AttrMask::RDEV),
            FATTR4_SPACE_USED => masks.contains(AttrMask::BLOCKS),
            FATTR4_TIME_ACCESS => masks.contains(AttrMask::ATIME),
            FATTR4_TIME_METADATA => masks.contains(AttrMask::CTIME),
            FATTR4_TIME_MODIFY => masks.contains(AttrMask::MTIME),
            _ => false,
        };
        if wanted {
            ids.push(id);
        }
    }
    ids
}

/// Parse a raw GETATTR attribute list encoded for the given ids (in id order).
pub(super) fn parse_attr_list(ids: &[u32], list: &[u8]) -> VfResult<AttrValues> {
    let mut v = AttrValues::default();
    let mut off = 0usize;
    for id in ids {
        let field = match *id {
            FATTR4_TYPE => {
                v.ftype = Some(read_u32(list, &mut off)?);
                continue;
            }
            FATTR4_CHANGE => {
                v.change = read_u64(list, &mut off)?;
                AttrMask::CHANGE
            }
            FATTR4_SIZE => {
                v.size = read_u64(list, &mut off)?;
                AttrMask::SIZE
            }
            FATTR4_FSID => {
                let major = read_u64(list, &mut off)?;
                let minor = read_u64(list, &mut off)?;
                v.fs_id = (u128::from(major) << 64) | u128::from(minor);
                AttrMask::FS_ID
            }
            FATTR4_NAMED_ATTR => {
                v.has_named_attr = read_u32(list, &mut off)? != 0;
                AttrMask::NAMED_ATTR
            }
            FATTR4_FILEID => {
                v.fileid = read_u64(list, &mut off)?;
                AttrMask::FILEID
            }
            FATTR4_MODE => {
                v.mode = read_u32(list, &mut off)?;
                AttrMask::MODE
            }
            FATTR4_NUMLINKS => {
                v.nlink = read_u32(list, &mut off)?;
                AttrMask::NLINK
            }
            FATTR4_OWNER => {
                let uid = crate::identity::name_to_id(read_str(list, &mut off)?, false);
                v.uid = uid.unwrap_or_default();
                v.present.set(AttrMask::UID, uid.is_some());
                continue;
            }
            FATTR4_OWNER_GROUP => {
                let gid = crate::identity::name_to_id(read_str(list, &mut off)?, true);
                v.gid = gid.unwrap_or_default();
                v.present.set(AttrMask::GID, gid.is_some());
                continue;
            }
            FATTR4_RAWDEV => {
                let major = read_u32(list, &mut off)?;
                let minor = read_u32(list, &mut off)?;
                v.rdev = libc::makedev(major, minor);
                AttrMask::RDEV
            }
            FATTR4_SPACE_USED => {
                v.blocks = read_u64(list, &mut off)? / 512;
                AttrMask::BLOCKS
            }
            FATTR4_TIME_ACCESS => {
                (v.atime_sec, v.atime_nsec) = read_nfstime(list, &mut off)?;
                AttrMask::ATIME
            }
            FATTR4_TIME_MODIFY => {
                (v.mtime_sec, v.mtime_nsec) = read_nfstime(list, &mut off)?;
                AttrMask::MTIME
            }
            FATTR4_TIME_METADATA => {
                (v.ctime_sec, v.ctime_nsec) = read_nfstime(list, &mut off)?;
                AttrMask::CTIME
            }
            _ => unreachable!(),
        };
        v.present.insert(field);
    }
    if off != list.len() {
        return Err(attr_decode_error());
    }
    Ok(v)
}

#[cfg(feature = "fuzzing")]
pub(crate) fn validate_attr_list(ids: &[u32], list: &[u8]) -> VfResult<()> {
    parse_attr_list(ids, list).map(|_| ())
}

/// `S_IFMT` type bits for an NFSv4 file type code.
fn s_ifmt(ftype: u32) -> u32 {
    match ftype {
        nfs_ftype4_NF4REG => 0o100000,  // S_IFREG
        nfs_ftype4_NF4DIR => 0o040000,  // S_IFDIR
        nfs_ftype4_NF4LNK => 0o120000,  // S_IFLNK
        nfs_ftype4_NF4BLK => 0o060000,  // S_IFBLK
        nfs_ftype4_NF4CHR => 0o020000,  // S_IFCHR
        nfs_ftype4_NF4FIFO => 0o010000, // S_IFIFO
        nfs_ftype4_NF4SOCK => 0o140000, // S_IFSOCK
        _ => 0,
    }
}

/// Fill `a` from parsed values where the mask requests the attribute. `mode`
/// is the permission bits plus the `S_IFMT` bits derived from `ftype`.
pub(super) fn apply_attrs(a: &mut VfAttrs, v: &AttrValues) {
    a.ftype = v
        .ftype
        .map(vfsi_core::file_type_from_nfs)
        .unwrap_or(VfType::Regular);
    a.returned = a.masks & v.present;
    if a.returned.contains(AttrMask::MODE) {
        a.mode = v.mode | s_ifmt(vfsi_core::file_type_to_nfs(&a.ftype));
    }
    // Copy only requested, actually present fields; leave absent payloads alone.
    macro_rules! copy {
        ($mask:ident, $($field:ident),+) => {
            if a.returned.contains(AttrMask::$mask) {
                $(a.$field = v.$field;)+
            }
        };
    }
    copy!(SIZE, size);
    copy!(NLINK, nlink);
    copy!(FILEID, fileid);
    copy!(FS_ID, fs_id);
    copy!(CHANGE, change);
    copy!(UID, uid);
    copy!(GID, gid);
    copy!(RDEV, rdev);
    // Already converted from SPACE_USED to 512-byte units during parsing.
    copy!(BLOCKS, blocks);
    copy!(MTIME, mtime_sec, mtime_nsec);
    copy!(ATIME, atime_sec, atime_nsec);
    copy!(CTIME, ctime_sec, ctime_nsec);
    copy!(NAMED_ATTR, has_named_attr);
}

/// Whether a single `FATTR4_TYPE` attribute payload names a symlink. A
/// malformed list is an error, never an implicit "not a symlink".
pub(super) fn type_is_symlink(attrs: &[u8]) -> VfResult<bool> {
    let mut off = 0;
    Ok(read_u32(attrs, &mut off)? == nfs_ftype4_NF4LNK)
}

fn read_u32(buf: &[u8], off: &mut usize) -> VfResult<u32> {
    let end = off.checked_add(4).ok_or_else(attr_decode_error)?;
    let bytes = buf.get(*off..end).ok_or_else(attr_decode_error)?;
    let v = u32::from_be_bytes(bytes.try_into().expect("four-byte slice"));
    *off = end;
    Ok(v)
}

fn read_u64(buf: &[u8], off: &mut usize) -> VfResult<u64> {
    let end = off.checked_add(8).ok_or_else(attr_decode_error)?;
    let bytes = buf.get(*off..end).ok_or_else(attr_decode_error)?;
    let v = u64::from_be_bytes(bytes.try_into().expect("eight-byte slice"));
    *off = end;
    Ok(v)
}

fn attr_decode_error() -> VfError {
    VfError::transport(0, "malformed NFS attribute list")
}

/// Read an XDR `nfstime4`: `int64 seconds; uint32 nseconds` (12 bytes).
fn read_nfstime(buf: &[u8], off: &mut usize) -> VfResult<(i64, u32)> {
    let end = off.checked_add(12).ok_or_else(attr_decode_error)?;
    let bytes = buf.get(*off..end).ok_or_else(attr_decode_error)?;
    let secs = i64::from_be_bytes(bytes[..8].try_into().expect("eight-byte slice"));
    let nsec = u32::from_be_bytes(bytes[8..].try_into().expect("four-byte slice"));
    *off = end;
    Ok((secs, nsec))
}

/// Read an XDR `utf8string`: length + padded bytes.
fn read_str<'a>(buf: &'a [u8], off: &mut usize) -> VfResult<&'a [u8]> {
    let len = read_u32(buf, off)? as usize;
    let padded = len.checked_add(3).ok_or_else(attr_decode_error)? & !3;
    let end = off.checked_add(padded).ok_or_else(attr_decode_error)?;
    let data_end = off.checked_add(len).ok_or_else(attr_decode_error)?;
    if end > buf.len() {
        return Err(attr_decode_error());
    }
    let s = &buf[*off..data_end];
    *off = end;
    Ok(s)
}

pub(super) fn filesystem_attributes() -> Vec<u32> {
    vec![
        FATTR4_FILES_AVAIL,
        FATTR4_FILES_FREE,
        FATTR4_FILES_TOTAL,
        FATTR4_MAXFILESIZE,
        FATTR4_MAXLINK,
        FATTR4_MAXNAME,
        FATTR4_SPACE_AVAIL,
        FATTR4_SPACE_FREE,
        FATTR4_SPACE_TOTAL,
    ]
}

pub(super) fn decode_filesystem_stats(bitmap: bitmap4, bytes: &[u8]) -> VfResult<FilesystemStats> {
    let words = bitmap
        .map
        .get(..bitmap.bitmap4_len as usize)
        .ok_or_else(attr_decode_error)?;
    let mut stats = FilesystemStats::default();
    let mut offset = 0;
    for (word, bits) in words.iter().enumerate() {
        for bit in 0..32 {
            if bits & (1 << bit) == 0 {
                continue;
            }
            let attribute = (word * 32 + bit) as u32;
            stats = match attribute {
                FATTR4_FILES_AVAIL => {
                    stats.with_available_files(Some(read_u64(bytes, &mut offset)?))
                }
                FATTR4_FILES_FREE => stats.with_free_files(Some(read_u64(bytes, &mut offset)?)),
                FATTR4_FILES_TOTAL => stats.with_total_files(Some(read_u64(bytes, &mut offset)?)),
                FATTR4_MAXFILESIZE => stats.with_max_file_size(Some(read_u64(bytes, &mut offset)?)),
                FATTR4_MAXLINK => {
                    stats.with_max_links(Some(u64::from(read_u32(bytes, &mut offset)?)))
                }
                FATTR4_MAXNAME => {
                    stats.with_max_name_len(Some(u64::from(read_u32(bytes, &mut offset)?)))
                }
                FATTR4_SPACE_AVAIL => {
                    stats.with_available_bytes(Some(read_u64(bytes, &mut offset)?))
                }
                FATTR4_SPACE_FREE => stats.with_free_bytes(Some(read_u64(bytes, &mut offset)?)),
                FATTR4_SPACE_TOTAL => stats.with_total_bytes(Some(read_u64(bytes, &mut offset)?)),
                _ => return Err(attr_decode_error()),
            };
        }
    }
    if offset != bytes.len() {
        return Err(attr_decode_error());
    }
    Ok(stats)
}

/// Whether a path SETATTR must guard against its final component being a
/// symlink before mutation. NVERIFY can enforce this within the same compound.
pub(super) fn path_setattrs_require_type_resolution(attrs: &[VfAttrs], follow: bool) -> bool {
    attrs.iter().any(|attrs| {
        !attrs.file.is_fd()
            && (attrs.masks.intersects(AttrMask::MODE | AttrMask::SIZE)
                || (follow && attrs.masks.intersects(AttrMask::ATIME | AttrMask::MTIME)))
    })
}

pub(super) fn nofollow_symlink_setattr_is_unsupported(
    follow: bool,
    ftype: u32,
    masks: AttrMask,
) -> bool {
    !follow && ftype == nfs_ftype4_NF4LNK && masks.intersects(AttrMask::MODE | AttrMask::SIZE)
}

impl NfsVecFs {
    /// The merged (single-compound) setattrsv: resolve each parent once,
    /// then LOOKUP + GETATTR type + SETATTR per file. Symlinks (either to
    /// refuse for lsetattrsv or to follow for setattrsv) are handled
    /// per-file via the phased path.
    pub(super) fn vsetattrs_nfs(&mut self, attrs: &[VfAttrs], follow: bool) -> VfRes {
        const SETTABLE: AttrMask = AttrMask::MODE
            .union(AttrMask::SIZE)
            .union(AttrMask::ATIME)
            .union(AttrMask::MTIME)
            .union(AttrMask::UID)
            .union(AttrMask::GID);
        if attrs.is_empty() {
            return Ok(());
        }
        for (i, a) in attrs.iter().enumerate() {
            let unsupported = a.masks.difference(SETTABLE);
            if !unsupported.is_empty() {
                return Err(VfError::unsupported(i));
            }
        }
        // Ownership updates still require name-to-id resolution before the
        // SETATTR compound. Other path updates use an in-compound NVERIFY guard
        // so a symlink stops before mutation, then follow it only when requested.
        if attrs
            .iter()
            .any(|a| a.masks.intersects(AttrMask::UID | AttrMask::GID))
        {
            return self.vsetattrs_phased_nfs(attrs, follow);
        }
        let mut ops = Vec::with_capacity(attrs.len());
        let mut symlink_guards = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            symlink_guards.push(path_setattrs_require_type_resolution(
                std::slice::from_ref(a),
                follow,
            ));
            let file = if a.file.is_fd() {
                let fd = a.file.fd().unwrap();
                let open = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
                crate::client::FileRef::Handle(open.fh.clone())
            } else {
                crate::client::FileRef::Path(
                    crate::path::path_bytes(
                        &self.server_vf_path(&a.file).map_err(|e| e.with_index(i))?,
                    )
                    .to_vec(),
                )
            };
            let mode = if a.masks.contains(AttrMask::MODE) {
                Some(a.mode & 0o7777)
            } else {
                None
            };
            let size = if a.masks.contains(AttrMask::SIZE) {
                Some(a.size)
            } else {
                None
            };
            let atime = a
                .masks
                .contains(AttrMask::ATIME)
                .then_some((a.atime_sec, a.atime_nsec));
            let mtime = a
                .masks
                .contains(AttrMask::MTIME)
                .then_some((a.mtime_sec, a.mtime_nsec));
            ops.push(crate::client::PathSetattrOp {
                file,
                mode,
                uid: a.masks.contains(AttrMask::UID).then_some(a.uid),
                gid: a.masks.contains(AttrMask::GID).then_some(a.gid),
                size,
                atime,
                mtime,
                check_type: false,
            });
        }
        let mut start = 0;
        while start < attrs.len() {
            match self
                .nfs
                .setattr_path_compound_guarded(&ops[start..], &symlink_guards[start..])
            {
                Ok(outcome) => {
                    if let Some((i, status)) = outcome.failed {
                        let i = start + i;
                        if symlink_guards[i] && status == nfsstat4_NFS4ERR_SAME {
                            if nofollow_symlink_setattr_is_unsupported(
                                follow,
                                nfs_ftype4_NF4LNK,
                                attrs[i].masks,
                            ) {
                                return Err(VfError::unsupported(i));
                            }
                            if follow {
                                // NVERIFY failed before this path's SETATTR; follow
                                // the final symlink and mutate only its target.
                                self.setattr_one_following(i, &attrs[i])?;
                                start = i + 1;
                                continue;
                            }
                        }
                        // The successful prefix is already applied. Resume at the
                        // first failed path, preserving its index and symlink policy.
                        if let Err(error) = self.vsetattrs_phased_nfs(&attrs[i..], follow) {
                            return Err(error.map_index(|relative| i + relative));
                        }
                        return Ok(());
                    }
                    return Ok(());
                }
                Err(error) if error.is_transport() => {
                    return Err(vfsi_core::error_from_rpc(error, None));
                }
                Err(_) => {
                    return self
                        .vsetattrs_phased_nfs(&attrs[start..], follow)
                        .map_err(|error| error.map_index(|relative| start + relative));
                }
            }
        }
        Ok(())
    }

    /// The legacy phased setattrsv (resolve_many_tcfile + setattr_many).
    fn vsetattrs_phased_nfs(&mut self, attrs: &[VfAttrs], follow: bool) -> VfRes {
        const SETTABLE: AttrMask = AttrMask::MODE
            .union(AttrMask::SIZE)
            .union(AttrMask::ATIME)
            .union(AttrMask::MTIME)
            .union(AttrMask::UID)
            .union(AttrMask::GID);
        for (i, a) in attrs.iter().enumerate() {
            let unsupported = a.masks.difference(SETTABLE);
            if !unsupported.is_empty() {
                return Err(VfError::unsupported(i));
            }
        }
        let files: Vec<&FileOperand> = attrs.iter().map(|a| &a.file).collect();
        let resolved = self.resolve_files_nfs(&files, follow)?;
        let mut ops = Vec::with_capacity(attrs.len());
        for (i, (a, resolution)) in attrs.iter().zip(resolved).enumerate() {
            let (fh, ftype) = resolution.map_err(|status| VfError::nfs(i, status))?;
            if nofollow_symlink_setattr_is_unsupported(follow, ftype, a.masks) {
                // NFSv4 has no non-following mode/size setter for symlinks;
                // refuse like the `std::fs` backend instead of pretending the
                // SETATTR applied to the link.
                return Err(VfError::unsupported(i));
            }
            let mode = if a.masks.contains(AttrMask::MODE) {
                Some(a.mode & 0o7777)
            } else {
                None
            };
            let size = if a.masks.contains(AttrMask::SIZE) {
                Some(a.size)
            } else {
                None
            };
            let atime = a
                .masks
                .contains(AttrMask::ATIME)
                .then_some((a.atime_sec, a.atime_nsec));
            let mtime = a
                .masks
                .contains(AttrMask::MTIME)
                .then_some((a.mtime_sec, a.mtime_nsec));
            ops.push(crate::client::SetattrOp {
                fh,
                mode,
                uid: a.masks.contains(AttrMask::UID).then_some(a.uid),
                gid: a.masks.contains(AttrMask::GID).then_some(a.gid),
                size,
                atime,
                mtime,
            });
        }
        self.nfs
            .setattr_many(&ops)
            .map_err(vfsi_core::error_from_rpc_indexed)
    }

    /// The merged (single-compound) getattrsv: resolve each parent once,
    /// then LOOKUP + GETATTR per file. Final-component symlinks (follow
    /// semantics) are resolved individually afterwards.
    pub(super) fn getattrsv_impl(&mut self, attrs: &mut [VfAttrs], follow: bool) -> VfRes {
        if attrs.is_empty() {
            return Ok(());
        }
        let mut ops = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            let file = if a.file.is_fd() {
                let fd = a.file.fd().unwrap();
                let open = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
                crate::client::FileRef::Handle(open.fh.clone())
            } else {
                crate::client::FileRef::Path(
                    crate::path::path_bytes(
                        &self.server_vf_path(&a.file).map_err(|e| e.with_index(i))?,
                    )
                    .to_vec(),
                )
            };
            ops.push(crate::client::PathGetattrOp {
                file,
                attrs: request_mask_to_attr_list(&a.masks),
            });
        }
        match self.nfs.getattr_path_compound(&ops) {
            Ok(outcome) => {
                if let Some((i, _st)) = outcome.failed {
                    // Prefix [0..i) succeeded; resume [i..] via the phased
                    // path, re-attributing its error to the original index.
                    let mut prefix_attrs = attrs[..i].to_vec();
                    for (k, a) in prefix_attrs.iter_mut().enumerate() {
                        let Some(list) = outcome.lists[k].as_deref() else {
                            return self.getattrsv_phased(attrs, follow);
                        };
                        let Ok(v) = parse_attr_list(&ops[k].attrs, list) else {
                            // A malformed merged result is not a valid
                            // completed prefix. Retry the read-only GETATTR
                            // through the independently decoded phased path.
                            return self.getattrsv_phased(attrs, follow);
                        };
                        apply_attrs(a, &v);
                        if follow && a.ftype == VfType::Symlink {
                            self.stat_one_following(k, a)?;
                        }
                    }
                    attrs[..i].clone_from_slice(&prefix_attrs);
                    if let Err(e) = self.getattrsv_phased(&mut attrs[i..], follow) {
                        return Err(e.map_index(|rel| i + rel));
                    }
                    return Ok(());
                }
                let mut symlinks = Vec::new();
                let mut parsed = Vec::with_capacity(attrs.len());
                for (i, op) in ops.iter().enumerate() {
                    let Some(list) = outcome.lists[i].as_deref() else {
                        return self.getattrsv_phased(attrs, follow);
                    };
                    let Ok(v) = parse_attr_list(&op.attrs, list) else {
                        return self.getattrsv_phased(attrs, follow);
                    };
                    parsed.push(v);
                }
                for (i, (a, v)) in attrs.iter_mut().zip(parsed).enumerate() {
                    apply_attrs(a, &v);
                    if follow && a.ftype == VfType::Symlink {
                        symlinks.push(i);
                    }
                }
                for i in symlinks {
                    self.stat_one_following(i, &mut attrs[i])?;
                }
                Ok(())
            }
            Err(error) if error.is_transport() => Err(vfsi_core::error_from_rpc(error, None)),
            Err(_) => self.getattrsv_phased(attrs, follow),
        }
    }

    /// The legacy phased getattrsv (resolve_many_tcfile + getattr_many).
    fn getattrsv_phased(&mut self, attrs: &mut [VfAttrs], follow: bool) -> VfRes {
        let files: Vec<&FileOperand> = attrs.iter().map(|a| &a.file).collect();
        let resolved = self.resolve_files_nfs(&files, follow)?;
        let mut ops = Vec::with_capacity(attrs.len());
        let mut first_failure: Option<VfError> = None;
        for (i, (a, resolution)) in attrs.iter().zip(resolved).enumerate() {
            let fh = match resolution {
                Ok((fh, _)) => fh,
                Err(status) => {
                    if first_failure.is_none() {
                        first_failure = Some(VfError::nfs(i, status));
                    }
                    continue;
                }
            };
            let ids = request_mask_to_attr_list(&a.masks);
            ops.push(crate::client::GetattrOp { fh, attrs: ids });
        }
        if let Some(e) = first_failure {
            return Err(e);
        }
        let results = self
            .nfs
            .getattr_many(&ops)
            .map_err(vfsi_core::error_from_rpc_indexed)?;
        for ((a, op), list) in attrs.iter_mut().zip(ops).zip(results) {
            let v = parse_attr_list(&op.attrs, &list)?;
            apply_attrs(a, &v);
        }
        Ok(())
    }

    /// stat one path following final-component symlinks (phased).
    fn stat_one_following(&mut self, index: usize, a: &mut VfAttrs) -> VfResult<()> {
        let path = self
            .server_vf_path(&a.file)
            .map_err(|e| e.with_index(index))?;
        let fh = self
            .resolve_follow(&path)
            .map_err(|e| e.with_index(index))?;
        let ids = request_mask_to_attr_list(&a.masks);
        let list = self
            .nfs
            .getattr(&fh, &ids)
            .map_err(|e| vfsi_core::error_from_rpc(e, index))?;
        let v = parse_attr_list(&ids, &list)?;
        apply_attrs(a, &v);
        Ok(())
    }

    /// setattr one path following final-component symlinks (phased).
    fn setattr_one_following(&mut self, index: usize, a: &VfAttrs) -> VfResult<()> {
        let path = self
            .server_vf_path(&a.file)
            .map_err(|e| e.with_index(index))?;
        let fh = self
            .resolve_follow(&path)
            .map_err(|e| e.with_index(index))?;
        let mode = if a.masks.contains(AttrMask::MODE) {
            Some(a.mode & 0o7777)
        } else {
            None
        };
        let size = if a.masks.contains(AttrMask::SIZE) {
            Some(a.size)
        } else {
            None
        };
        let atime = a
            .masks
            .contains(AttrMask::ATIME)
            .then_some((a.atime_sec, a.atime_nsec));
        let mtime = a
            .masks
            .contains(AttrMask::MTIME)
            .then_some((a.mtime_sec, a.mtime_nsec));
        self.nfs
            .setattr_ownership(
                &fh,
                mode,
                size,
                (
                    a.masks.contains(AttrMask::UID).then_some(a.uid),
                    a.masks.contains(AttrMask::GID).then_some(a.gid),
                ),
                atime,
                mtime,
            )
            .map_err(|e| vfsi_core::error_from_rpc(e, index))
    }

    /// The size in bytes of `fh`.
    pub(super) fn file_size(&mut self, fh: &NfsFh) -> VfResult<u64> {
        let list = self
            .nfs
            .getattr(fh, &[FATTR4_SIZE])
            .map_err(|e| vfsi_core::error_from_rpc(e, 0))?;
        let mut off = 0;
        read_u64(&list, &mut off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xdr_string(value: &[u8]) -> Vec<u8> {
        let mut wire = (value.len() as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(value);
        wire.resize(4 + value.len().div_ceil(4) * 4, 0);
        wire
    }

    #[test]
    fn compact_decoder_preserves_sparse_values_and_requested_masks() {
        assert!(std::mem::size_of::<AttrValues>() <= 128);
        for value in [0, u64::MAX] {
            let id = (value as u32).to_string();
            let time = |seconds: i64, nanos: u32| {
                let mut bytes = seconds.to_be_bytes().to_vec();
                bytes.extend_from_slice(&nanos.to_be_bytes());
                bytes
            };
            let mut fs_id = value.to_be_bytes().to_vec();
            fs_id.extend_from_slice(&value.to_be_bytes());
            let mut device = (value as u32).to_be_bytes().to_vec();
            device.extend_from_slice(&(value as u32).to_be_bytes());
            let fields = [
                (
                    FATTR4_TYPE,
                    AttrMask::empty(),
                    nfs_ftype4_NF4LNK.to_be_bytes().to_vec(),
                ),
                (
                    FATTR4_CHANGE,
                    AttrMask::CHANGE,
                    value.to_be_bytes().to_vec(),
                ),
                (FATTR4_SIZE, AttrMask::SIZE, value.to_be_bytes().to_vec()),
                (
                    FATTR4_NAMED_ATTR,
                    AttrMask::NAMED_ATTR,
                    u32::from(value != 0).to_be_bytes().to_vec(),
                ),
                (FATTR4_FSID, AttrMask::FS_ID, fs_id),
                (
                    FATTR4_FILEID,
                    AttrMask::FILEID,
                    value.to_be_bytes().to_vec(),
                ),
                (
                    FATTR4_MODE,
                    AttrMask::MODE,
                    (value as u32).to_be_bytes().to_vec(),
                ),
                (
                    FATTR4_NUMLINKS,
                    AttrMask::NLINK,
                    (value as u32).to_be_bytes().to_vec(),
                ),
                (FATTR4_OWNER, AttrMask::UID, xdr_string(id.as_bytes())),
                (FATTR4_OWNER_GROUP, AttrMask::GID, xdr_string(id.as_bytes())),
                (FATTR4_RAWDEV, AttrMask::RDEV, device),
                (
                    FATTR4_SPACE_USED,
                    AttrMask::BLOCKS,
                    value.to_be_bytes().to_vec(),
                ),
                (FATTR4_TIME_ACCESS, AttrMask::ATIME, time(-1, 500_000_000)),
                (FATTR4_TIME_METADATA, AttrMask::CTIME, time(0, 0)),
                (FATTR4_TIME_MODIFY, AttrMask::MTIME, time(1, 250_000_000)),
            ];
            for (id, _, bytes) in &fields {
                for cut in 0..bytes.len() {
                    assert!(parse_attr_list(&[*id], &bytes[..cut]).is_err());
                }
            }
            let full = VfAttrs {
                size: value,
                mode: value as u32,
                nlink: value as u32,
                fileid: value,
                fs_id: if value == 0 { 0 } else { u128::MAX },
                change: value,
                uid: value as u32,
                gid: value as u32,
                rdev: libc::makedev(value as u32, value as u32),
                blocks: value / 512,
                atime_sec: -1,
                atime_nsec: 500_000_000,
                mtime_sec: 1,
                mtime_nsec: 250_000_000,
                ctime_sec: 0,
                ctime_nsec: 0,
                has_named_attr: value != 0,
                ..VfAttrs::default()
            };
            // Empty, all fields, and every singleton detect accidental sharing
            // of presence bits and TYPE's independence from the MODE field.
            for selected in [0, (1u16 << fields.len()) - 1]
                .into_iter()
                .chain((0..fields.len()).map(|i| 1u16 << i))
            {
                let mut ids = Vec::new();
                let mut wire = Vec::new();
                let mut present = AttrMask::empty();
                for (index, (id, mask, bytes)) in fields.iter().enumerate() {
                    if selected & (1 << index) != 0 {
                        ids.push(*id);
                        wire.extend_from_slice(bytes);
                        present |= *mask;
                    }
                }
                let decoded = parse_attr_list(&ids, &wire).unwrap();
                for requested in [AttrMask::empty(), AttrMask::all()] {
                    let mut actual = VfAttrs {
                        masks: requested,
                        ..VfAttrs::default()
                    };
                    apply_attrs(&mut actual, &decoded);
                    let mut expected = full.clone();
                    expected.returned = present & requested;
                    expected.ftype = if selected & 1 != 0 {
                        VfType::Symlink
                    } else {
                        VfType::Regular
                    };
                    expected.size = if expected.returned.contains(AttrMask::SIZE) {
                        value
                    } else {
                        0
                    };
                    expected.mode = if expected.returned.contains(AttrMask::MODE) {
                        value as u32 | s_ifmt(vfsi_core::file_type_to_nfs(&expected.ftype))
                    } else {
                        0
                    };
                    assert_eq!(actual.returned, expected.returned);
                    assert_eq!(
                        vfsi_core::metadata_from_attrs(actual),
                        vfsi_core::metadata_from_attrs(expected),
                        "{ids:?} {requested:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn unmapped_owner_clears_presence_without_changing_other_payloads() {
        for (id, mask) in [
            (FATTR4_OWNER, AttrMask::UID),
            (FATTR4_OWNER_GROUP, AttrMask::GID),
        ] {
            let known = xdr_string(b"0");
            let values = parse_attr_list(&[id], &known).unwrap();
            assert!(values.present.contains(mask)); // uid/gid zero is known.
            let mut wire = known;
            wire.extend_from_slice(&xdr_string(&[0xff])); // No NSS lookup.
            let values = parse_attr_list(&[id, id], &wire).unwrap();
            assert!(!values.present.contains(mask));
            let mut attrs = VfAttrs {
                masks: AttrMask::all(),
                returned: AttrMask::all(),
                uid: 42,
                gid: 43,
                size: 123,
                atime_sec: -7,
                atime_nsec: 99,
                ..VfAttrs::default()
            };
            apply_attrs(&mut attrs, &values);
            assert_eq!(attrs.returned, AttrMask::empty());
            assert_eq!(
                (
                    attrs.uid,
                    attrs.gid,
                    attrs.size,
                    attrs.atime_sec,
                    attrs.atime_nsec
                ),
                (42, 43, 123, -7, 99)
            );
        }
    }

    #[test]
    fn allocation_attribute_strings_borrow_wire_and_check_padding() {
        for length in 0..10u32 {
            let mut wire = length.to_be_bytes().to_vec();
            wire.extend(std::iter::repeat_n(0xff, length as usize));
            wire.resize(4 + (length as usize).div_ceil(4) * 4, 0);
            let field_end = wire.len();
            wire.extend_from_slice(&17u32.to_be_bytes());
            let mut offset = 0;
            let bytes = read_str(&wire, &mut offset).unwrap();
            assert_eq!(bytes, &wire[4..4 + length as usize]);
            assert_eq!(bytes.as_ptr(), wire[4..].as_ptr());
            assert_eq!(offset, field_end);
            assert_eq!(read_u32(&wire, &mut offset).unwrap(), 17);
            assert_eq!(offset, wire.len());
            for cut in 0..field_end {
                assert!(read_str(&wire[..cut], &mut 0).is_err());
            }
        }
        let wire = [
            0, 0, 0, 4, b' ', b'4', b'2', b' ', 0, 0, 0, 2, b'9', b'9', 0, 0,
        ];
        let values = parse_attr_list(&[FATTR4_OWNER, FATTR4_OWNER_GROUP], &wire).unwrap();
        let mut attrs = VfAttrs {
            masks: AttrMask::UID | AttrMask::GID,
            ..VfAttrs::default()
        };
        apply_attrs(&mut attrs, &values);
        assert_eq!(attrs.returned, AttrMask::UID | AttrMask::GID);
        assert_eq!(attrs.uid, 42);
        assert_eq!(attrs.gid, 99);
    }
    #[test]
    fn phased_metadata_preserves_grouping_masks_duplicates_and_error_indices() {
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "2".into())
            .parse()
            .unwrap();
        let mut backend = NfsVecFs::connect_minor(&server, minor).unwrap();
        use vfsi_sync::test_support::borrow_writes;
        let root = format!("/vfsi-phased-metadata-{}", std::process::id());
        for parent in ["a", "b"] {
            backend
                .ensure_dir_impl(Path::new(&format!("{root}/{parent}")), 0o755)
                .unwrap();
        }
        let paths = [
            format!("{root}/a/one"),
            format!("{root}/b/two"),
            format!("{root}/a/three"),
        ];
        let writes: Vec<_> = paths
            .iter()
            .zip([b"a".as_slice(), b"bb", b"ccc"])
            .map(|(path, data)| {
                vfsi_sync::WriteOp::from_path(path, VfOffset::At(0), data.to_vec())
                    .with_creation()
                    .with_truncate()
            })
            .collect();
        backend.vwrite_impl(&borrow_writes(&writes)).unwrap();
        let mut attrs: Vec<_> = [
            (2, AttrMask::SIZE),
            (1, AttrMask::MODE),
            (0, AttrMask::SIZE | AttrMask::MODE),
            (2, AttrMask::identity()),
        ]
        .into_iter()
        .map(|(index, masks)| VfAttrs {
            file: FileOperand::from_path(&paths[index]),
            masks,
            ..Default::default()
        })
        .collect();
        #[cfg(feature = "test-faults")]
        let _ = crate::compound::thread_compound_stats();
        backend.getattrsv_phased(&mut attrs, true).unwrap();
        #[cfg(feature = "test-faults")]
        assert_eq!(
            crate::compound::thread_compound_stats().0,
            7,
            "resolve and type-check each unique parent once, lookup its children together, then fetch attributes together"
        );
        assert_eq!(attrs[0].size, 3);
        assert_eq!(attrs[2].size, 1);
        assert_eq!(attrs[1].ftype, VfType::Regular);
        assert_ne!(attrs[3].fileid, 0);
        for attr in &attrs {
            assert!(attr.returned.contains(attr.masks));
        }
        let mut invalid: Vec<_> = [
            &paths[0],
            &format!("{root}/missing/file"),
            &format!("{root}/b/missing"),
        ]
        .into_iter()
        .map(|path| VfAttrs {
            file: FileOperand::from_path(path),
            masks: AttrMask::SIZE,
            ..Default::default()
        })
        .collect();
        let error = backend.getattrsv_phased(&mut invalid, true).unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        backend
            .remove_paths_impl(&[Path::new(&root)], true)
            .unwrap();
    }
}
