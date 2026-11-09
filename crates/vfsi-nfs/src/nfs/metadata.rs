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
    VfError::invalid_reply(0, "malformed NFS attribute list")
}

/// Read an XDR `nfstime4`: `int64 seconds; uint32 nseconds` (12 bytes).
fn read_nfstime(buf: &[u8], off: &mut usize) -> VfResult<(i64, u32)> {
    let end = off.checked_add(12).ok_or_else(attr_decode_error)?;
    let bytes = buf.get(*off..end).ok_or_else(attr_decode_error)?;
    let secs = i64::from_be_bytes(bytes[..8].try_into().expect("eight-byte slice"));
    let nsec = u32::from_be_bytes(bytes[8..].try_into().expect("four-byte slice"));
    if nsec >= 1_000_000_000 {
        return Err(attr_decode_error());
    }
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
    /// Preserve independent updates while ownership-dependent resolution
    /// remains a separate phase. Ordinary updates retain their merged path.
    pub(super) fn vsetattrs_nfs(
        &mut self,
        attrs: &[VfAttrs],
        follow: bool,
    ) -> vfsi_core::VecResult<()> {
        self.ensure_writable(attrs.len())?;
        if attrs
            .iter()
            .all(|attr| !attr.masks.intersects(AttrMask::UID | AttrMask::GID))
        {
            return self.vsetattrs_merged_nfs(attrs, follow);
        }
        let mut output = Vec::with_capacity(attrs.len());
        let mut start = 0;
        while start < attrs.len() {
            let phased = attrs[start].masks.intersects(AttrMask::UID | AttrMask::GID);
            let count = attrs[start..]
                .iter()
                .take_while(|attr| attr.masks.intersects(AttrMask::UID | AttrMask::GID) == phased)
                .count();
            let result = if phased {
                self.vsetattrs_phased_nfs(&attrs[start..start + count], follow)
            } else {
                self.vsetattrs_merged_nfs(&attrs[start..start + count], follow)
            };
            match result {
                Ok(results) => output.extend(results),
                Err(cause) => output.extend(
                    (0..count).map(|_| Err(vfsi_core::OpError::not_attempted(cause.clone()))),
                ),
            }
            start += count;
        }
        Ok(output)
    }

    fn vsetattrs_merged_nfs(
        &mut self,
        attrs: &[VfAttrs],
        follow: bool,
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        const SETTABLE: AttrMask = AttrMask::MODE
            .union(AttrMask::SIZE)
            .union(AttrMask::ATIME)
            .union(AttrMask::MTIME)
            .union(AttrMask::UID)
            .union(AttrMask::GID);
        let mut slots = (0..attrs.len()).map(|_| None).collect::<Vec<_>>();
        let mut ops = Vec::with_capacity(attrs.len());
        let mut indices = Vec::with_capacity(attrs.len());
        let mut guards = Vec::with_capacity(attrs.len());
        for (index, attr) in attrs.iter().enumerate() {
            let file = if !attr.masks.difference(SETTABLE).is_empty() {
                Err(VfError::unsupported(None))
            } else if let Some(fd) = attr.file.fd() {
                self.open_files
                    .get(&fd)
                    .map(|open| crate::client::FileRef::Handle(open.fh.clone()))
                    .ok_or_else(|| VfError::errno(None, ERR_EBADF))
            } else {
                self.server_vf_path(&attr.file)
                    .map(|path| crate::client::FileRef::Path(path_bytes(&path).to_vec()))
            };
            match file {
                Ok(file) => {
                    indices.push(index);
                    guards.push(path_setattrs_require_type_resolution(
                        std::slice::from_ref(attr),
                        follow,
                    ));
                    ops.push(crate::client::PathSetattrOp {
                        file,
                        mode: attr
                            .masks
                            .contains(AttrMask::MODE)
                            .then_some(attr.mode & 0o7777),
                        size: attr.masks.contains(AttrMask::SIZE).then_some(attr.size),
                        uid: None,
                        gid: None,
                        atime: attr
                            .masks
                            .contains(AttrMask::ATIME)
                            .then_some((attr.atime_sec, attr.atime_nsec)),
                        mtime: attr
                            .masks
                            .contains(AttrMask::MTIME)
                            .then_some((attr.mtime_sec, attr.mtime_nsec)),
                        check_type: false,
                    });
                }
                Err(cause) => slots[index] = Some(Err(OpError::not_attempted(cause))),
            }
        }
        let mut start = 0;
        while start < ops.len() {
            let (results, repair) = match self
                .nfs
                .setattr_path_compound_guarded(&ops[start..], &guards[start..])
            {
                Ok(result) => result,
                Err(cause) => {
                    for &index in &indices[start..] {
                        slots[index] = Some(Err(OpError::not_attempted(cause.clone())));
                    }
                    break;
                }
            };
            let repair = repair.map(|point| point.input);
            let final_symlink = repair.is_some_and(|index| {
                results
                    .get(index)
                    .and_then(|result| result.as_ref().err())
                    .is_some_and(|error| {
                        error.cause().status()
                            == Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_SAME))
                    })
            });
            let completed = repair.unwrap_or(ops.len() - start);
            for (offset, result) in results.into_iter().take(completed).enumerate() {
                slots[indices[start + offset]] = Some(result.map(|_| ()));
            }
            let Some(relative) = repair else { break };
            let index = indices[start + relative];
            let cause = if final_symlink
                && nofollow_symlink_setattr_is_unsupported(
                    follow,
                    nfs_ftype4_NF4LNK,
                    attrs[index].masks,
                ) {
                Some(OpError::not_attempted(VfError::unsupported(None)))
            } else {
                None
            };
            slots[index] = Some(match cause {
                Some(error) => Err(error),
                None => {
                    match self.vsetattrs_phased_nfs(std::slice::from_ref(&attrs[index]), follow) {
                        Ok(mut results) if results.len() == 1 => results.pop().unwrap(),
                        Ok(_) => Err(OpError::unknown(
                            VfError::invalid_reply(None, "invalid SETATTR repair count"),
                            None,
                        )),
                        Err(cause) => Err(OpError::not_attempted(cause)),
                    }
                }
            });
            start += relative + 1;
        }
        Ok(slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "missing SETATTR outcome"),
                        None,
                    ))
                })
            })
            .collect())
    }

    /// Resolve only valid candidates, then submit all independent SETATTRs.
    fn vsetattrs_phased_nfs(
        &mut self,
        attrs: &[VfAttrs],
        follow: bool,
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        const SETTABLE: AttrMask = AttrMask::MODE
            .union(AttrMask::SIZE)
            .union(AttrMask::ATIME)
            .union(AttrMask::MTIME)
            .union(AttrMask::UID)
            .union(AttrMask::GID);
        let mut slots = (0..attrs.len()).map(|_| None).collect::<Vec<_>>();
        let mut candidates = Vec::with_capacity(attrs.len());
        let mut refs = Vec::with_capacity(attrs.len());
        for (index, attr) in attrs.iter().enumerate() {
            if !attr.masks.difference(SETTABLE).is_empty() {
                slots[index] = Some(Err(OpError::not_attempted(VfError::unsupported(None))));
            } else {
                candidates.push(index);
                refs.push(&attr.file);
            }
        }
        let resolved = self.resolve_files_nfs(&refs, follow)?;
        let mut ops = Vec::with_capacity(attrs.len());
        let mut indices = Vec::with_capacity(attrs.len());
        for (index, resolution) in candidates.into_iter().zip(resolved) {
            let attr = &attrs[index];
            match resolution {
                Ok((_, kind))
                    if nofollow_symlink_setattr_is_unsupported(follow, kind, attr.masks) =>
                {
                    slots[index] = Some(Err(OpError::not_attempted(VfError::unsupported(None))));
                }
                Ok((fh, _)) => {
                    indices.push(index);
                    ops.push(crate::client::SetattrOp {
                        fh,
                        mode: attr
                            .masks
                            .contains(AttrMask::MODE)
                            .then_some(attr.mode & 0o7777),
                        size: attr.masks.contains(AttrMask::SIZE).then_some(attr.size),
                        uid: attr.masks.contains(AttrMask::UID).then_some(attr.uid),
                        gid: attr.masks.contains(AttrMask::GID).then_some(attr.gid),
                        atime: attr
                            .masks
                            .contains(AttrMask::ATIME)
                            .then_some((attr.atime_sec, attr.atime_nsec)),
                        mtime: attr
                            .masks
                            .contains(AttrMask::MTIME)
                            .then_some((attr.mtime_sec, attr.mtime_nsec)),
                    });
                }
                Err(error) => slots[index] = Some(Err(OpError::not_attempted(error.into_cause()))),
            }
        }
        let results = match self.nfs.setattr_many(&ops) {
            Ok(results) => results,
            Err(cause) => ops
                .iter()
                .map(|_| Err(OpError::not_attempted(cause.clone())))
                .collect(),
        };
        for (index, result) in indices.into_iter().zip(results) {
            slots[index] = Some(result);
        }
        Ok(slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "missing phased SETATTR outcome"),
                        None,
                    ))
                })
            })
            .collect())
    }

    /// The merged (single-compound) getattrsv: resolve each parent once,
    /// then LOOKUP + GETATTR per file. Final-component symlinks (follow
    /// semantics) are resolved individually afterwards.
    pub(super) fn getattr_with_recovery(
        &mut self,
        attrs: &mut [VfAttrs],
        follow: bool,
    ) -> vfsi_core::VecResult<()> {
        if self.recovery_in_progress {
            return self.getattrsv_impl(attrs, follow);
        }
        self.vector_read_with_recovery(
            |_, _| true,
            |client, indices| match indices {
                None => client.getattrsv_impl(attrs, follow),
                Some(indices) => {
                    let mut pending: Vec<_> =
                        indices.iter().map(|&index| attrs[index].clone()).collect();
                    let outcomes = client.getattrsv_impl(&mut pending, follow)?;
                    for ((&index, attr), outcome) in indices.iter().zip(pending).zip(&outcomes) {
                        if outcome.is_ok() {
                            attrs[index] = attr;
                        }
                    }
                    Ok(outcomes)
                }
            },
        )
    }

    pub(super) fn getattrsv_impl(
        &mut self,
        attrs: &mut [VfAttrs],
        follow: bool,
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        if attrs.is_empty() {
            return Ok(Vec::new());
        }
        let mut slots: Vec<Option<vfsi_core::OpResult<()>>> =
            (0..attrs.len()).map(|_| None).collect();
        let mut ops = Vec::with_capacity(attrs.len());
        let mut indices = Vec::with_capacity(attrs.len());
        for (index, attr) in attrs.iter().enumerate() {
            let file = match &attr.file {
                FileOperand::Fd(fd) => self
                    .open_files
                    .get(fd)
                    .map(|open| crate::client::FileRef::Handle(open.fh.clone()))
                    .ok_or_else(|| VfError::errno(index, ERR_EBADF)),
                _ => self.server_vf_path(&attr.file).map(|path| {
                    crate::client::FileRef::Path(vfsi_core::path::path_bytes(&path).to_vec())
                }),
            };
            match file {
                Ok(file) => {
                    indices.push(index);
                    ops.push(crate::client::PathGetattrOp {
                        file,
                        attrs: request_mask_to_attr_list(&attr.masks),
                    });
                }
                Err(error) => slots[index] = Some(Err(OpError::not_attempted(error))),
            }
        }
        let results = match self.nfs.getattr_path_compound(&ops) {
            Ok(results) => results,
            Err(error) => {
                for index in indices {
                    slots[index] = Some(Err(OpError::not_attempted(error.clone())));
                }
                return Ok(slots
                    .into_iter()
                    .map(|slot| slot.expect("metadata input assigned"))
                    .collect());
            }
        };

        let mut fallback = Vec::new();
        for ((index, op), result) in indices.into_iter().zip(ops).zip(results) {
            let operation = result
                .and_then(|list| {
                    parse_attr_list(&op.attrs, &list)
                        .map_err(|error| vfsi_core::OpError::failed(error, None))
                })
                .and_then(|values| {
                    apply_attrs(&mut attrs[index], &values);
                    if follow && attrs[index].ftype == VfType::Symlink {
                        self.stat_one_following(index, &mut attrs[index])
                            .map_err(|error| {
                                if error.is_transport() {
                                    vfsi_core::OpError::unknown(error, None)
                                } else {
                                    vfsi_core::OpError::failed(error, None)
                                }
                            })
                    } else {
                        Ok(())
                    }
                });
            if operation.as_ref().is_err_and(|error| {
                !error.cause().is_transport()
                    && matches!(
                        error.cause().status(),
                        Some(vfsi_core::StatusCode::Nfs(
                            nfsstat4_NFS4ERR_INVAL | nfsstat4_NFS4ERR_SYMLINK
                        ))
                    )
            }) {
                fallback.push(index);
            }
            slots[index] = Some(operation);
        }
        if !fallback.is_empty() {
            // Root paths and ancestor symlinks share a batched phased fallback.
            // Never repeat successful siblings or flatten this into singleton RPCs.
            let mut pending: Vec<_> = fallback.iter().map(|&index| attrs[index].clone()).collect();
            let results = self.getattr_phased_results(&mut pending, follow)?;
            for ((index, attr), result) in fallback.into_iter().zip(pending).zip(results) {
                if result.is_ok() {
                    attrs[index] = attr;
                }
                slots[index] = Some(result);
            }
        }
        Ok(slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "missing merged attribute outcome"),
                        None,
                    ))
                })
            })
            .collect())
    }

    /// Resolve failed merged inputs together and retain their individual results.
    fn getattr_phased_results(
        &mut self,
        attrs: &mut [VfAttrs],
        follow: bool,
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        let files: Vec<_> = attrs.iter().map(|a| &a.file).collect();
        let resolved = match self.resolve_files_nfs(&files, follow) {
            Ok(resolved) => resolved,
            Err(error) => {
                return Ok(attrs
                    .iter()
                    .map(|_| Err(OpError::unknown(error.clone(), None)))
                    .collect());
            }
        };
        let mut outcomes: Vec<Option<vfsi_core::OpResult<()>>> =
            (0..attrs.len()).map(|_| None).collect();
        let mut indices = Vec::with_capacity(attrs.len());
        let mut ops = Vec::with_capacity(attrs.len());
        for (index, resolution) in resolved.into_iter().enumerate() {
            match resolution {
                Ok((fh, _)) => {
                    indices.push(index);
                    ops.push(crate::client::GetattrOp {
                        fh,
                        attrs: request_mask_to_attr_list(&attrs[index].masks),
                    });
                }
                Err(error) => outcomes[index] = Some(Err(error)),
            }
        }
        match self.nfs.getattr_many_with_bitmap(&ops) {
            Ok(results) => {
                for ((index, op), result) in indices.into_iter().zip(ops).zip(results) {
                    outcomes[index] = Some(result.and_then(|(_, bytes)| {
                        parse_attr_list(&op.attrs, &bytes)
                            .map(|values| apply_attrs(&mut attrs[index], &values))
                            .map_err(|error| OpError::failed(error, None))
                    }));
                }
            }
            Err(error) => {
                for index in indices {
                    outcomes[index] = Some(Err(OpError::not_attempted(error.clone())));
                }
            }
        }
        Ok(outcomes
            .into_iter()
            .map(|result| {
                result.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "missing phased metadata result"),
                        None,
                    ))
                })
            })
            .collect())
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
    fn timestamp_decoder_rejects_noncanonical_nanoseconds() {
        for attribute in [FATTR4_TIME_ACCESS, FATTR4_TIME_METADATA, FATTR4_TIME_MODIFY] {
            for seconds in [-1i64, 0, 1] {
                for nanos in [0u32, 999_999_999, 1_000_000_000, u32::MAX] {
                    let mut wire = seconds.to_be_bytes().to_vec();
                    wire.extend_from_slice(&nanos.to_be_bytes());
                    let result = parse_attr_list(&[attribute], &wire);
                    if nanos < 1_000_000_000 {
                        let values = result.unwrap();
                        let actual = match attribute {
                            FATTR4_TIME_ACCESS => (values.atime_sec, values.atime_nsec),
                            FATTR4_TIME_METADATA => (values.ctime_sec, values.ctime_nsec),
                            FATTR4_TIME_MODIFY => (values.mtime_sec, values.mtime_nsec),
                            _ => unreachable!(),
                        };
                        assert_eq!(actual, (seconds, nanos));
                    } else {
                        let error = result.unwrap_err();
                        assert_eq!(
                            error.transport_kind(),
                            Some(vfsi_core::TransportKind::InvalidReply)
                        );
                    }
                }
            }
        }
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
        backend
            .vwrite_impl(&borrow_writes(&writes))
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
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
        vfsi_core::internal::collect_vec_result(
            backend.getattr_phased_results(&mut attrs, true),
            attrs.len(),
        )
        .unwrap();
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
        let error = vfsi_core::internal::collect_vec_result(
            backend.getattr_phased_results(&mut invalid, true),
            invalid.len(),
        )
        .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        backend
            .remove_paths_impl(&[Path::new(&root)], true)
            .unwrap();
    }
}
