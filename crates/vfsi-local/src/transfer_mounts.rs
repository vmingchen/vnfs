//! Detect transfer overlap concealed by Linux mount aliases.
use crate::open_rooted_path_with_flags;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use vfsi_core::IndexedError;
use vfsi_core::internal::{TransferRoot, errors};

struct Mount {
    id: u64,
    device: (u32, u32),
    root: PathBuf,
    point: PathBuf,
}

struct Extent<'a> {
    mount: &'a Mount,
    path: PathBuf,
    directory: bool,
}

impl Extent<'_> {
    fn overlap_path<'a>(&'a self, other: &'a Self) -> Option<&'a Path> {
        if self.mount.id == other.mount.id || self.mount.device != other.mount.device {
            return None;
        }
        if self.path == other.path || (self.directory && other.path.starts_with(&self.path)) {
            Some(&other.path)
        } else if other.directory && self.path.starts_with(&other.path) {
            Some(&self.path)
        } else {
            None
        }
    }
}

pub(super) fn check(
    anchor: &File,
    roots: &[TransferRoot<'_>],
    max_path_bytes: usize,
) -> Result<(), IndexedError> {
    let mounts = read_mounts().map_err(IndexedError::unattributed)?;
    let namespace = fs::read_link(fd_path(anchor)).map_err(IndexedError::unattributed)?;
    let mut devices = HashSet::new();
    let aliases: HashSet<_> = mounts
        .iter()
        .filter(|mount| namespace.starts_with(&mount.point) || mount.point.starts_with(&namespace))
        .filter(|mount| !devices.insert(mount.device))
        .map(|mount| mount.device)
        .collect();
    if aliases.is_empty() {
        return Ok(());
    }

    let view = View {
        anchor,
        namespace: &namespace,
        mounts: &mounts,
        aliases: &aliases,
    };
    let mut sources = Vec::new();
    let mut source_bytes = 0;
    for root in roots {
        view.project(
            root.source,
            root.directory,
            &mut source_bytes,
            max_path_bytes,
            &mut sources,
        )
        .map_err(|error| attributed(root.index, root.source, error))?;
    }
    let mut checked = HashSet::new();
    let mut destinations = Vec::new();
    for root in roots {
        if !checked.insert((root.destination, root.directory)) {
            continue;
        }
        destinations.clear();
        let mut destination_bytes = 0;
        view.project(
            root.destination,
            root.directory,
            &mut destination_bytes,
            max_path_bytes,
            &mut destinations,
        )
        .map_err(|error| attributed(root.index, root.destination, error))?;
        for destination in &destinations {
            for source in &sources {
                if view
                    .overlaps_alias(destination, source)
                    .map_err(|error| attributed(root.index, root.destination, error))?
                {
                    return Err(attributed(
                        root.index,
                        root.destination,
                        io::Error::new(
                            ErrorKind::InvalidInput,
                            "transfer overlaps a source through a mount alias",
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

struct View<'a> {
    anchor: &'a File,
    namespace: &'a Path,
    mounts: &'a [Mount],
    aliases: &'a HashSet<(u32, u32)>,
}

impl<'a> View<'a> {
    fn overlaps_alias(&self, destination: &Extent<'_>, source: &Extent<'_>) -> io::Result<bool> {
        let Some(overlap) = destination.overlap_path(source) else {
            return Ok(false);
        };
        // A lower mount's nominal extent includes paths that can be hidden
        // by an overmount. Recheck both actual routes to the intersection.
        for extent in [destination, source] {
            let relative = overlap
                .strip_prefix(&extent.mount.root)
                .expect("mount coordinate");
            let host = extent.mount.point.join(relative);
            let path = host
                .strip_prefix(self.namespace)
                .map_err(|_| io::Error::other("transfer overlap is outside the held namespace"))?;
            let (file, _) = self.open_or_parent(path)?;
            if mount_id(&file)? != extent.mount.id {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn project(
        &self,
        path: &Path,
        directory: bool,
        bytes: &mut usize,
        max_path_bytes: usize,
        extents: &mut Vec<Extent<'a>>,
    ) -> io::Result<()> {
        let (file, existing) = self.open_or_parent(path)?;
        let suffix = path.strip_prefix(existing).expect("existing ancestor");
        let directory = directory && (!suffix.as_os_str().is_empty() || file.metadata()?.is_dir());
        let host = fs::read_link(fd_path(&file))?;
        let id = mount_id(&file)?;
        let mount = self
            .mounts
            .iter()
            .find(|mount| mount.id == id)
            .ok_or_else(|| io::Error::other("transfer mount disappeared"))?;
        if self.aliases.contains(&mount.device) {
            let relative = host
                .strip_prefix(&mount.point)
                .map_err(|_| io::Error::other("transfer path is outside its covering mount"))?;
            let projected = mount.root.join(relative).join(suffix);
            record(
                extents,
                bytes,
                max_path_bytes,
                Extent {
                    mount,
                    path: projected,
                    directory,
                },
            )?;
        }
        if !directory || !suffix.as_os_str().is_empty() {
            return Ok(());
        }
        for nested in self.mounts.iter().filter(|candidate| {
            self.aliases.contains(&candidate.device)
                && candidate.point != host
                && candidate.point.starts_with(&host)
        }) {
            let Ok(path) = nested.point.strip_prefix(self.namespace) else {
                continue;
            };
            let file = match open_rooted_path_with_flags(self.anchor, path, libc::O_NOFOLLOW) {
                Ok(file) => file,
                Err(error)
                    if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            if mount_id(&file)? == nested.id {
                record(
                    extents,
                    bytes,
                    max_path_bytes,
                    Extent {
                        mount: nested,
                        path: nested.root.clone(),
                        directory: file.metadata()?.is_dir(),
                    },
                )?;
            }
        }
        Ok(())
    }

    fn open_or_parent<'p>(&self, path: &'p Path) -> io::Result<(File, &'p Path)> {
        for (depth, ancestor) in path.ancestors().enumerate() {
            // The operand's final entry is never followed by transfer.
            // Existing parents of an absent operand must resolve as directories.
            let flags = if depth == 0 {
                libc::O_NOFOLLOW
            } else {
                libc::O_DIRECTORY
            };
            match open_rooted_path_with_flags(self.anchor, ancestor, flags) {
                Ok(file) => return Ok((file, ancestor)),
                Err(error)
                    if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
                Err(error) => return Err(error),
            }
        }
        Err(ErrorKind::NotFound.into())
    }
}

fn record<'a>(
    extents: &mut Vec<Extent<'a>>,
    bytes: &mut usize,
    max_path_bytes: usize,
    extent: Extent<'a>,
) -> io::Result<()> {
    *bytes = bytes.saturating_add(extent.path.as_os_str().len());
    if *bytes > max_path_bytes {
        return Err(io::Error::from_raw_os_error(libc::EFBIG));
    }
    extents.push(extent);
    Ok(())
}

fn attributed(index: usize, path: &Path, error: io::Error) -> IndexedError {
    IndexedError::at(
        index,
        errors::with_context(error, "transfer mount ancestry", path),
    )
}

fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn mount_id(file: &File) -> io::Result<u64> {
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .and_then(|id| id.trim().parse().ok())
        .ok_or_else(|| io::Error::new(ErrorKind::Unsupported, "mount ID unavailable"))
}

fn read_mounts() -> io::Result<Vec<Mount>> {
    let text = fs::read("/proc/self/mountinfo")?;
    text.split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            parse_mount(line)
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "invalid mountinfo record"))
        })
        .collect()
}

fn parse_mount(line: &[u8]) -> Option<Mount> {
    let mut fields = line.split(|&byte| byte == b' ');
    let id = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    fields.next()?; // Parent mount ID.
    let (major, minor) = std::str::from_utf8(fields.next()?).ok()?.split_once(':')?;
    Some(Mount {
        id,
        device: (major.parse().ok()?, minor.parse().ok()?),
        root: mount_path(fields.next()?)?,
        point: mount_path(fields.next()?)?,
    })
}

fn mount_path(field: &[u8]) -> Option<PathBuf> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut bytes = field.iter().copied();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'\\' {
            match [bytes.next()?, bytes.next()?, bytes.next()?] {
                [b'0', b'4', b'0'] => b' ',
                [b'0', b'1', b'1'] => b'\t',
                [b'0', b'1', b'2'] => b'\n',
                [b'1', b'3', b'4'] => b'\\',
                _ => return None,
            }
        } else {
            byte
        });
    }
    let path = PathBuf::from(OsString::from_vec(decoded));
    path.is_absolute().then_some(path)
}
