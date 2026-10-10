//! NFS attribute decoding and metadata operations.

use super::*;

// ---------------------------------------------------------------------------
// NFS attribute parsing
// ---------------------------------------------------------------------------

/// Parsed values of a GETATTR reply for the supported FATTR4 attributes.
#[derive(Debug, Clone, Default)]
pub(super) struct AttrValues {
    pub(super) ftype: Option<u32>,
    pub(super) change: Option<u64>,
    pub(super) mode: Option<u32>,
    pub(super) size: Option<u64>,
    pub(super) nlink: Option<u32>,
    pub(super) fileid: Option<u64>,
    pub(super) uid: Option<u32>,
    pub(super) gid: Option<u32>,
    pub(super) rdev: Option<u64>,
    pub(super) blocks: Option<u64>,
    pub(super) mtime: Option<(i64, u32)>,
    pub(super) atime: Option<(i64, u32)>,
    pub(super) ctime: Option<(i64, u32)>,
    pub(super) has_named_attr: Option<bool>,
}

/// The full set of supported FATTR4 ids, in wire (increasing) order. Must
/// match `crate::client::READDIR_ATTRS`.
const FULL_ATTR_IDS: [u32; 14] = [
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

pub(super) fn request_mask_to_attr_list(masks: &AttrMask) -> Vec<u32> {
    let mut ids = Vec::new();
    for id in FULL_ATTR_IDS {
        let wanted = match id {
            FATTR4_TYPE => true, // always fetch type (cheap, aids listdir)
            FATTR4_CHANGE => masks.contains(AttrMask::CHANGE),
            FATTR4_SIZE => masks.contains(AttrMask::SIZE),
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
        match *id {
            FATTR4_TYPE => {
                v.ftype = Some(read_u32(list, &mut off)?);
            }
            FATTR4_CHANGE => {
                v.change = Some(read_u64(list, &mut off)?);
            }
            FATTR4_SIZE => {
                v.size = Some(read_u64(list, &mut off)?);
            }
            FATTR4_NAMED_ATTR => {
                v.has_named_attr = Some(read_u32(list, &mut off)? != 0);
            }
            FATTR4_FILEID => {
                v.fileid = Some(read_u64(list, &mut off)?);
            }
            FATTR4_MODE => {
                v.mode = Some(read_u32(list, &mut off)?);
            }
            FATTR4_NUMLINKS => {
                v.nlink = Some(read_u32(list, &mut off)?);
            }
            FATTR4_OWNER => {
                v.uid = crate::identity::name_to_id(&read_str(list, &mut off)?, false);
            }
            FATTR4_OWNER_GROUP => {
                v.gid = crate::identity::name_to_id(&read_str(list, &mut off)?, true);
            }
            FATTR4_RAWDEV => {
                let major = read_u32(list, &mut off)?;
                let minor = read_u32(list, &mut off)?;
                v.rdev = Some(libc::makedev(major, minor));
            }
            FATTR4_SPACE_USED => {
                v.blocks = Some(read_u64(list, &mut off)? / 512);
            }
            FATTR4_TIME_ACCESS => {
                v.atime = Some(read_nfstime(list, &mut off)?);
            }
            FATTR4_TIME_MODIFY => {
                v.mtime = Some(read_nfstime(list, &mut off)?);
            }
            FATTR4_TIME_METADATA => {
                v.ctime = Some(read_nfstime(list, &mut off)?);
            }
            _ => unreachable!(),
        }
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
    a.returned = AttrMask::empty();
    if a.masks.contains(AttrMask::MODE)
        && let Some(mode) = v.mode
    {
        a.mode = mode | s_ifmt(vfsi_core::file_type_to_nfs(&a.ftype));
        a.returned.insert(AttrMask::MODE);
    }
    if a.masks.contains(AttrMask::SIZE)
        && let Some(size) = v.size
    {
        a.size = size;
        a.returned.insert(AttrMask::SIZE);
    }
    if a.masks.contains(AttrMask::NLINK)
        && let Some(nlink) = v.nlink
    {
        a.nlink = nlink;
        a.returned.insert(AttrMask::NLINK);
    }
    if a.masks.contains(AttrMask::FILEID)
        && let Some(fileid) = v.fileid
    {
        a.fileid = fileid;
        a.returned.insert(AttrMask::FILEID);
    }
    if a.masks.contains(AttrMask::CHANGE)
        && let Some(change) = v.change
    {
        a.change = change;
        a.returned.insert(AttrMask::CHANGE);
    }
    if a.masks.contains(AttrMask::UID)
        && let Some(uid) = v.uid
    {
        a.uid = uid;
        a.returned.insert(AttrMask::UID);
    }
    if a.masks.contains(AttrMask::GID)
        && let Some(gid) = v.gid
    {
        a.gid = gid;
        a.returned.insert(AttrMask::GID);
    }
    if a.masks.contains(AttrMask::RDEV)
        && let Some(rdev) = v.rdev
    {
        a.rdev = rdev;
        a.returned.insert(AttrMask::RDEV);
    }
    if a.masks.contains(AttrMask::BLOCKS)
        && let Some(blocks) = v.blocks
    {
        // `v.blocks` is already FATTR4_SPACE_USED converted to 512-byte
        // units in `parse_attr_list` (matching the dummy's `st_blocks`).
        a.blocks = blocks;
        a.returned.insert(AttrMask::BLOCKS);
    }
    if a.masks.contains(AttrMask::MTIME)
        && let Some((s, n)) = v.mtime
    {
        a.mtime_sec = s;
        a.mtime_nsec = n;
        a.returned.insert(AttrMask::MTIME);
    }
    if a.masks.contains(AttrMask::ATIME)
        && let Some((s, n)) = v.atime
    {
        a.atime_sec = s;
        a.atime_nsec = n;
        a.returned.insert(AttrMask::ATIME);
    }
    if a.masks.contains(AttrMask::CTIME)
        && let Some((s, n)) = v.ctime
    {
        a.ctime_sec = s;
        a.ctime_nsec = n;
        a.returned.insert(AttrMask::CTIME);
    }
    if a.masks.contains(AttrMask::NAMED_ATTR)
        && let Some(has) = v.has_named_attr
    {
        a.has_named_attr = has;
        a.returned.insert(AttrMask::NAMED_ATTR);
    }
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
fn read_str(buf: &[u8], off: &mut usize) -> VfResult<Vec<u8>> {
    let len = read_u32(buf, off)? as usize;
    let padded = len.checked_add(3).ok_or_else(attr_decode_error)? & !3;
    let end = off.checked_add(padded).ok_or_else(attr_decode_error)?;
    let data_end = off.checked_add(len).ok_or_else(attr_decode_error)?;
    if end > buf.len() {
        return Err(attr_decode_error());
    }
    let s = buf[*off..data_end].to_vec();
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
            match attribute {
                FATTR4_FILES_AVAIL => stats.available_files = Some(read_u64(bytes, &mut offset)?),
                FATTR4_FILES_FREE => stats.free_files = Some(read_u64(bytes, &mut offset)?),
                FATTR4_FILES_TOTAL => stats.total_files = Some(read_u64(bytes, &mut offset)?),
                FATTR4_MAXFILESIZE => stats.max_file_size = Some(read_u64(bytes, &mut offset)?),
                FATTR4_MAXLINK => stats.max_links = Some(u64::from(read_u32(bytes, &mut offset)?)),
                FATTR4_MAXNAME => {
                    stats.max_name_len = Some(u64::from(read_u32(bytes, &mut offset)?))
                }
                FATTR4_SPACE_AVAIL => stats.available_bytes = Some(read_u64(bytes, &mut offset)?),
                FATTR4_SPACE_FREE => stats.free_bytes = Some(read_u64(bytes, &mut offset)?),
                FATTR4_SPACE_TOTAL => stats.total_bytes = Some(read_u64(bytes, &mut offset)?),
                _ => return Err(attr_decode_error()),
            }
        }
    }
    if offset != bytes.len() {
        return Err(attr_decode_error());
    }
    Ok(stats)
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
        if attrs
            .iter()
            .any(|a| a.masks.intersects(AttrMask::UID | AttrMask::GID))
        {
            return self.vsetattrs_phased_nfs(attrs, follow);
        }
        let mut ops = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            let file = if a.file.is_descriptor() {
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
                check_type: true,
            });
        }
        match self.nfs.setattr_path_compound(&ops) {
            Ok(outcome) => {
                if let Some((i, _st)) = outcome.failed {
                    // Prefix [0..i) succeeded; resume [i..] via the phased
                    // path, re-attributing its error to the original index.
                    for (k, a) in attrs.iter().enumerate().take(i) {
                        let own_type = outcome.types[k];
                        if own_type == Some(nfs_ftype4_NF4LNK) {
                            if !follow {
                                return Err(VfError::unsupported(k));
                            }
                            self.setattr_one_following(k, a)?;
                        }
                    }
                    if let Err(e) = self.vsetattrs_phased_nfs(&attrs[i..], follow) {
                        return Err(e.map_index(|rel| i + rel));
                    }
                    return Ok(());
                }
                for (i, a) in attrs.iter().enumerate() {
                    let own_type = outcome.types[i];
                    if own_type == Some(nfs_ftype4_NF4LNK) {
                        if !follow {
                            // No non-following mode/size setter for symlinks.
                            return Err(VfError::unsupported(i));
                        }
                        self.setattr_one_following(i, a)?;
                    }
                }
                Ok(())
            }
            Err(error) if error.is_transport() => Err(vfsi_core::error_from_rpc(error, None)),
            Err(_) => self.vsetattrs_phased_nfs(attrs, follow),
        }
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
        let files: Vec<&VfFile> = attrs.iter().map(|a| &a.file).collect();
        let resolved = self.resolve_files_nfs(&files, follow)?;
        let mut ops = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            let (fh, ftype) = match &resolved[i] {
                Ok(x) => x.clone(),
                Err(status) => return Err(VfError::nfs(i, *status)),
            };
            if !follow
                && ftype == nfs_ftype4_NF4LNK
                && !a.masks.difference(AttrMask::UID | AttrMask::GID).is_empty()
            {
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
            let file = if a.file.is_descriptor() {
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
        let files: Vec<&VfFile> = attrs.iter().map(|a| &a.file).collect();
        let resolved = self.resolve_files_nfs(&files, follow)?;
        let mut ops = Vec::with_capacity(attrs.len());
        let mut ids_list = Vec::with_capacity(attrs.len());
        let mut first_failure: Option<VfError> = None;
        for (i, a) in attrs.iter().enumerate() {
            let fh = match &resolved[i] {
                Ok((fh, _)) => fh.clone(),
                Err(status) => {
                    if first_failure.is_none() {
                        first_failure = Some(VfError::nfs(i, *status));
                    }
                    ids_list.push(Vec::new());
                    continue;
                }
            };
            let ids = request_mask_to_attr_list(&a.masks);
            ids_list.push(ids.clone());
            ops.push(crate::client::GetattrOp { fh, attrs: ids });
        }
        if let Some(e) = first_failure {
            return Err(e);
        }
        let results = self
            .nfs
            .getattr_many(&ops)
            .map_err(vfsi_core::error_from_rpc_indexed)?;
        for ((a, ids), list) in attrs.iter_mut().zip(ids_list).zip(results) {
            let v = parse_attr_list(&ids, &list)?;
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
    pub(super) fn file_size(&mut self, fh: &WireFileHandle) -> VfResult<u64> {
        let list = self
            .nfs
            .getattr(fh, &[FATTR4_SIZE])
            .map_err(|e| vfsi_core::error_from_rpc(e, 0))?;
        let mut off = 0;
        read_u64(&list, &mut off)
    }
}
