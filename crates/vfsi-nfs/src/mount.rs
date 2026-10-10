//! Discover a direct NFSv4 connection from a directory in a Linux mount.

use std::collections::HashSet;
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use vfsi_sync::backend::VectorBackend;

use vfsi_core::{AttrMask, VfAttrs, VfError, VfFile, VfResult};

use crate::{NfsAuthentication, NfsVecFs};

/// Parsed supported NFS mount, shared with the mount-aware router.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountInfo {
    pub id: u64,
    pub mount_point: PathBuf,
    pub export: PathBuf,
    pub server: String,
    pub minor: u32,
    pub read_only: bool,
}

/// A discovered connection configuration pinned to one directory and mount.
/// Discovery supports NFSv4.1/4.2 over TCP with AUTH_SYS. It does not share
/// the kernel client's caches, state, or locks.
#[derive(Clone, Debug)]
pub struct NfsMount {
    info: MountInfo,
    local_path: PathBuf,
    root: PathBuf,
    device: u64,
    inode: u64,
    credentials: AuthSysIdentity,
}

impl NfsMount {
    pub fn discover(path: impl AsRef<Path>) -> VfResult<Self> {
        if !path.as_ref().is_absolute() {
            return Err(VfError::client(0, libc::EINVAL as u32)
                .with_context("mount path must be absolute", path.as_ref()));
        }
        let path = fs::canonicalize(path.as_ref()).map_err(|error| {
            VfError::client(0, error.raw_os_error().unwrap_or(libc::EIO) as u32)
                .with_context("from_mount", path.as_ref())
        })?;
        let metadata = fs::metadata(&path).map_err(|error| {
            VfError::client(0, error.raw_os_error().unwrap_or(libc::EIO) as u32)
                .with_context("from_mount", &path)
        })?;
        if !metadata.is_dir() {
            return Err(VfError::client(0, libc::ENOTDIR as u32).with_context("from_mount", &path));
        }
        let id = path_mount_id(&path).ok_or_else(|| unsupported(&path))?;
        let table = fs::read("/proc/self/mountinfo").map_err(|_| unsupported(&path))?;
        let info = find_mount(&table, id, &path)?;
        let suffix = path
            .strip_prefix(&info.mount_point)
            .map_err(|_| unsupported(&path))?;
        let root = info.export.join(suffix);
        let credentials = AuthSysIdentity::current().ok_or_else(|| unsupported(&path))?;
        Ok(Self {
            info,
            local_path: path,
            root,
            device: metadata.dev(),
            inode: metadata.ino(),
            credentials,
        })
    }

    pub fn host(&self) -> &str {
        &self.info.server
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn local_path(&self) -> &Path {
        &self.local_path
    }
    pub fn mount_point(&self) -> &Path {
        &self.info.mount_point
    }
    pub fn export_root(&self) -> &Path {
        &self.info.export
    }
    pub fn minor_version(&self) -> u32 {
        self.info.minor
    }
    pub fn read_only(&self) -> bool {
        self.info.read_only
    }

    pub(crate) fn check_local(&self) -> VfResult<()> {
        let metadata = fs::metadata(&self.local_path).map_err(|_| self.stale())?;
        if path_mount_id(&self.local_path) != Some(self.info.id)
            || metadata.dev() != self.device
            || metadata.ino() != self.inode
            || AuthSysIdentity::current().as_ref() != Some(&self.credentials)
        {
            return Err(self.stale());
        }
        let table = fs::read("/proc/self/mountinfo").map_err(|_| self.stale())?;
        if find_mount(&table, self.info.id, &self.local_path)? != self.info {
            return Err(self.stale());
        }
        Ok(())
    }

    pub(crate) fn validate_options(
        &self,
        host: &str,
        options: &crate::NfsConnectOptions,
    ) -> VfResult<()> {
        if host != self.host()
            || options.root != self.root
            || options.minorversion != Some(self.minor_version())
            || options.authentication != NfsAuthentication::AuthSys
        {
            return Err(VfError::client(0, libc::EINVAL as u32)
                .with_context("conflicting mount connection options", &self.local_path));
        }
        self.check_local()
    }

    pub(crate) fn verify(&self, filesystem: &mut NfsVecFs) -> VfResult<()> {
        let mut attrs = [VfAttrs {
            file: VfFile::from_os_path(Path::new("/")),
            masks: AttrMask::FILEID,
            ..VfAttrs::default()
        }];
        filesystem.vgetattrs_impl(&mut attrs)?;
        if !attrs[0].masks.contains(AttrMask::FILEID) {
            return Err(self.stale());
        }
        self.verify_file_id(attrs[0].fileid)?;
        self.check_local()
    }

    fn verify_file_id(&self, file_id: u64) -> VfResult<()> {
        if file_id != self.inode {
            Err(self.stale())
        } else {
            Ok(())
        }
    }

    fn stale(&self) -> VfError {
        VfError::client(0, libc::ESTALE as u32).with_context(
            "mount directory identity changed or remote root differs",
            &self.local_path,
        )
    }
}

fn unsupported(path: &Path) -> VfError {
    VfError::client(0, libc::EOPNOTSUPP as u32).with_context(
        "from_mount requires an unambiguous NFSv4.1/4.2 TCP sec=sys mount without nested mounts",
        path,
    )
}

fn find_mount(table: &[u8], id: u64, path: &Path) -> VfResult<MountInfo> {
    let mut selected = None;
    for line in table.split(|byte| *byte == b'\n') {
        let fields: Vec<_> = line.split(|byte| *byte == b' ').collect();
        if let Some(point) = fields.get(4).and_then(|field| decode_mount_field(field))
            && point != path
            && point.starts_with(path)
        {
            return Err(unsupported(path));
        }
        if fields
            .first()
            .and_then(|field| std::str::from_utf8(field).ok())
            .and_then(|value| value.parse::<u64>().ok())
            == Some(id)
        {
            if selected.is_some() {
                return Err(unsupported(path));
            }
            selected = parse_mount(line);
        }
    }
    selected
        .filter(|info| path.starts_with(&info.mount_point))
        .ok_or_else(|| unsupported(path))
}

/// Parse only connection settings that can be reproduced by a direct client.
#[doc(hidden)]
pub fn parse_mount(line: &[u8]) -> Option<MountInfo> {
    let fields: Vec<_> = line.split(|byte| *byte == b' ').collect();
    let separator = fields.iter().position(|field| *field == b"-")?;
    if separator < 6
        || fields.len() != separator + 4
        || !matches!(fields[separator + 1], b"nfs" | b"nfs4")
        || decode_mount_field(fields[3])? != Path::new("/")
    {
        return None;
    }
    let flags: HashSet<_> = fields[5].split(|byte| *byte == b',').collect();
    if flags.contains(b"ro".as_slice()) == flags.contains(b"rw".as_slice()) {
        return None;
    }
    let options: Vec<_> = fields[separator + 3].split(|byte| *byte == b',').collect();
    let value = |key: &[u8]| -> Option<&[u8]> {
        let mut found = options.iter().filter_map(|option| option.strip_prefix(key));
        let result = found.next()?;
        if found.next().is_some() {
            None
        } else {
            Some(result)
        }
    };
    if value(b"sec=")? != b"sys"
        || value(b"proto=")? != b"tcp"
        || options
            .iter()
            .any(|option| option.starts_with(b"xprtsec=") && *option != b"xprtsec=none")
    {
        return None;
    }
    let minor = match value(b"vers=")? {
        b"4.1" => 1,
        b"4.2" => 2,
        _ => return None,
    };
    let source = decode_mount_field(fields[separator + 2])?;
    let source = source.to_str()?;
    let (host, export) = source.rsplit_once(':')?;
    if host.is_empty() || !export.starts_with('/') {
        return None;
    }
    let export = PathBuf::from(export);
    if export
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return None;
    }
    let address: IpAddr = std::str::from_utf8(value(b"addr=")?)
        .ok()?
        .trim_matches(['[', ']'])
        .parse()
        .ok()?;
    let port = if options.iter().any(|option| option.starts_with(b"port=")) {
        std::str::from_utf8(value(b"port=")?)
            .ok()?
            .parse::<u16>()
            .ok()?
    } else {
        2049
    };
    if port == 0 {
        return None;
    }
    Some(MountInfo {
        id: std::str::from_utf8(fields[0]).ok()?.parse().ok()?,
        mount_point: decode_mount_field(fields[4])?,
        export,
        server: SocketAddr::new(address, port).to_string(),
        minor,
        read_only: flags.contains(b"ro".as_slice()) || options.contains(&b"ro".as_slice()),
    })
}

#[doc(hidden)]
pub fn decode_mount_field(field: &[u8]) -> Option<PathBuf> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        if field[index] == b'\\' {
            let digits = field.get(index + 1..index + 4)?;
            if !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                return None;
            }
            let value = u16::from(digits[0] - b'0') * 64
                + u16::from(digits[1] - b'0') * 8
                + u16::from(digits[2] - b'0');
            decoded.push(u8::try_from(value).ok()?);
            index += 4;
        } else {
            decoded.push(field[index]);
            index += 1;
        }
    }
    if decoded.contains(&0) {
        return None;
    }
    Some(std::ffi::OsString::from_vec(decoded).into())
}

#[doc(hidden)]
pub fn path_mount_id(path: &Path) -> Option<u64> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            path.as_ptr(),
            0,
            libc::STATX_MNT_ID,
            &mut stat,
        )
    };
    if result != 0 || stat.stx_mask & libc::STATX_MNT_ID == 0 {
        None
    } else {
        Some(stat.stx_mnt_id)
    }
}

/// Identity that a separate AUTH_SYS connection can faithfully reproduce.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthSysIdentity {
    uid: libc::uid_t,
    gid: libc::gid_t,
    groups: Vec<libc::gid_t>,
}

impl AuthSysIdentity {
    pub fn current() -> Option<Self> {
        let fsuid = unsafe { libc::setfsuid(!0) };
        let fsgid = unsafe { libc::setfsgid(!0) };
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        if fsuid as libc::uid_t != uid || fsgid as libc::gid_t != gid {
            return None;
        }
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        if !(0..=16).contains(&count) {
            return None;
        }
        let mut groups = vec![0; count as usize];
        if unsafe { libc::getgroups(count, groups.as_mut_ptr()) } != count {
            return None;
        }
        Some(Self { uid, gid, groups })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const LINE: &[u8] = b"46 49 0:45 / /mnt/nfs rw,relatime - nfs4 server:/export rw,vers=4.2,proto=tcp,sec=sys,addr=192.0.2.7";

    #[test]
    fn directory_mapping_and_read_only_are_preserved() {
        let info = find_mount(LINE, 46, Path::new("/mnt/nfs/git/tree")).unwrap();
        assert_eq!(
            info.export.join(
                Path::new("/mnt/nfs/git/tree")
                    .strip_prefix(&info.mount_point)
                    .unwrap()
            ),
            Path::new("/export/git/tree")
        );
        assert_eq!(info.server, "192.0.2.7:2049");
        assert!(!info.read_only);
        let ro = String::from_utf8(LINE.to_vec())
            .unwrap()
            .replace(" rw,relatime", " ro,relatime");
        assert!(parse_mount(ro.as_bytes()).unwrap().read_only);
    }

    #[test]
    fn unsupported_security_transport_and_ambiguous_endpoints_fail_closed() {
        let line = std::str::from_utf8(LINE).unwrap();
        for (old, new) in [
            ("sec=sys", "sec=krb5"),
            ("sec=sys", "sec=sys:krb5"),
            ("sec=sys", "sec=sys,sec=krb5"),
            ("proto=tcp", "proto=udp"),
            ("vers=4.2", "vers=4.0"),
            (" / /mnt", " /sub /mnt"),
            ("addr=192.0.2.7", "addr=192.0.2.7,addr=192.0.2.8"),
            ("sec=sys", "sec=sys,xprtsec=tls"),
            ("sec=sys", "sec=sys,port=0"),
            ("sec=sys", "sec=sys,port=bad"),
            ("sec=sys", "sec=sys,port=2049,port=2050"),
        ] {
            assert!(
                parse_mount(line.replace(old, new).as_bytes()).is_none(),
                "{new}"
            );
        }
        assert!(find_mount(LINE, 99, Path::new("/mnt/nfs")).is_err());
        assert!(find_mount(LINE, 46, Path::new("/mnt/nfs-other")).is_err());
    }

    #[test]
    fn escapes_ipv6_and_nested_mounts_are_handled() {
        assert_eq!(
            decode_mount_field(br"/mnt/with\040space").unwrap(),
            Path::new("/mnt/with space")
        );
        assert!(decode_mount_field(br"/mnt/bad\000").is_none());
        assert!(decode_mount_field(br"/mnt/bad\777").is_none());
        let line = std::str::from_utf8(LINE)
            .unwrap()
            .replace("addr=192.0.2.7", "addr=2001:db8::7,port=2050");
        assert_eq!(
            parse_mount(line.as_bytes()).unwrap().server,
            "[2001:db8::7]:2050"
        );
        let table = [LINE, b"\n47 46 0:46 / /mnt/nfs/nested rw - tmpfs none rw"].concat();
        assert!(find_mount(&table, 46, Path::new("/mnt/nfs")).is_err());
        assert!(find_mount(&table, 47, Path::new("/mnt/nfs/nested")).is_err());
    }

    #[test]
    fn local_paths_and_files_are_rejected_without_network_io() {
        assert!(NfsMount::discover(std::env::temp_dir()).is_err());
        assert!(NfsMount::discover("/proc/self/mountinfo").is_err());
    }

    #[test]
    fn relative_mount_paths_are_rejected_before_resolution() {
        for path in ["", ".", "data", "../data"] {
            assert_eq!(
                NfsMount::discover(path).unwrap_err().err_no(),
                libc::EINVAL as u32,
                "{path:?}"
            );
        }
    }

    #[test]
    fn wrong_remote_identity_and_connection_overrides_are_rejected() {
        let info = parse_mount(LINE).unwrap();
        let mount = NfsMount {
            root: info.export.clone(),
            local_path: info.mount_point.clone(),
            info,
            device: 1,
            inode: 123,
            credentials: AuthSysIdentity {
                uid: 0,
                gid: 0,
                groups: vec![],
            },
        };
        assert!(mount.verify_file_id(123).is_ok());
        assert_eq!(
            mount.verify_file_id(124).unwrap_err().err_no(),
            libc::ESTALE as u32
        );
        assert!(
            mount
                .validate_options("different-server", &crate::NfsConnectOptions::default())
                .is_err()
        );
    }
}
