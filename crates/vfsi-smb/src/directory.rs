//! Enumeration retains an open directory. Supplementary Unix-symlink probes
//! still require path opens: validate the server-reported path against that
//! handle's identity before and after probing. An unresolvable renamed path
//! returns ESTALE; it must never redirect probes into a replacement directory.
use super::*;
use smb2::client::DirectoryEntry;
mod reader;
use reader::DirectoryReader;
use std::collections::VecDeque;

fn validate_entry_name(name: &str) -> VfRes {
    if name.is_empty() || matches!(name, "." | "..") || name.bytes().any(|b| b == b'/' || b == 0) {
        return Err(VfError::invalid_reply(
            None,
            "invalid SMB directory entry name",
        ));
    }
    Ok(())
}

struct SmbDirectoryCursor {
    reader: DirectoryReader,
    buffered: VecDeque<DirectoryEntry>,
    directory: PathBuf,
    probe_files: bool,
}

fn directory_attrs(directory: &Path, entry: DirectoryEntry, masks: AttrMask) -> VfResult<VfAttrs> {
    validate_entry_name(&entry.name)?;
    // Preserve advertised reparse flags. Some servers omit these for Unix
    // symlinks; the bounded fallback probe fills them before projection.
    let reparse = entry.attributes.is_reparse_point();
    let mut attrs = VfAttrs {
        file: FileOperand::from_os_path(&Path::new("/").join(directory).join(&entry.name)),
        masks,
        ftype: if reparse {
            VfType::Symlink
        } else if entry.is_directory {
            VfType::Directory
        } else {
            VfType::Regular
        },
        ..VfAttrs::default()
    };
    if masks.contains(AttrMask::MODE) {
        attrs.mode = if reparse {
            libc::S_IFLNK | 0o777
        } else if entry.is_directory {
            libc::S_IFDIR | 0o777
        } else {
            libc::S_IFREG | 0o666
        };
        attrs.returned.insert(AttrMask::MODE);
    }
    if masks.contains(AttrMask::SIZE) {
        attrs.size = entry.size;
        attrs.returned.insert(AttrMask::SIZE);
    }
    if masks.contains(AttrMask::MTIME) {
        (attrs.mtime_sec, attrs.mtime_nsec) = filetime_parts(entry.modified);
        attrs.returned.insert(AttrMask::MTIME);
    }
    if masks.contains(AttrMask::FILEID)
        && let Some(index) = entry.index_number
    {
        attrs.fileid = index;
        attrs.returned.insert(AttrMask::FILEID);
    }
    Ok(attrs)
}

impl SmbVecFs {
    pub(super) fn probe_reparse_point_batch(
        &mut self,
        requests: &[(usize, CreateRequest)],
        reparse_points: &mut [bool],
    ) -> VfRes {
        if requests.is_empty() {
            return Ok(());
        }
        let connection = self.client.connection_mut().clone();
        let tree = self.tree.clone();
        let jobs = requests.iter().map(|(_, create)| {
            open_close_on_connection(connection.clone(), tree.clone(), create.clone())
        });
        let outcomes = self.runtime.block_on(join_all(jobs));
        queue_deferred_closes(&outcomes, &mut self.deferred_closes, |id| {
            DeferredClose::new(self.tree.clone(), id)
        });
        for ((index, _), outcome) in requests.iter().zip(outcomes) {
            reparse_points[*index] = outcome.result?;
        }
        Ok(())
    }

    fn verify_probe_directory(&mut self, path: &Path, identity: u64) -> VfRes {
        let mut request = self.open_request(&self.path_string(path)?, libc::O_RDONLY)?;
        // This is an identity check, so a root operand reached through an
        // ordinary directory symlink must retain its normal follow semantics.
        request.create_options = 1; // FILE_DIRECTORY_FILE.
        request.desired_access =
            FileAccessMask::new(FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE);
        let connection = self.client.connection_mut().clone();
        let outcome = self.runtime.block_on(concurrent_file_identity(
            connection,
            self.tree.clone(),
            request,
        ));
        if let Some(file_id) = outcome.deferred_close {
            self.deferred_closes
                .push(DeferredClose::new(self.tree.clone(), file_id));
        }
        match outcome.result {
            Ok(actual) if actual == identity => Ok(()),
            Ok(_) => Err(VfError::errno(0, libc::ESTALE as u32)
                .with_message("SMB directory probe path no longer names the retained directory")),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Err(VfError::errno(0, libc::ESTALE as u32)
                    .with_message(format!("SMB directory probe path is unavailable: {error}")))
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn read_directory_page(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        cursor: Option<DirPageCursor>,
        page_size: usize,
    ) -> VfResult<(Vec<VfAttrs>, Option<DirPageCursor>)> {
        self.read_directory_page_with_policy(dir, masks, cursor, page_size, false)
    }

    pub(super) fn read_directory_page_with_policy(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        probe_files: bool,
    ) -> VfResult<(Vec<VfAttrs>, Option<DirPageCursor>)> {
        if page_size == 0 {
            return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
        }
        let directory = self.abs_path(dir);
        let mut state = match cursor {
            Some(cursor) => {
                let state = cursor.into_state::<SmbDirectoryCursor>()?;
                if state.directory != directory || state.probe_files != probe_files {
                    return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
                }
                state
            }
            None => {
                let path = self.path_string(&directory)?;
                let reader = DirectoryReader::open(self, &path, masks.contains(AttrMask::FILEID))?;
                SmbDirectoryCursor {
                    reader,
                    buffered: VecDeque::new(),
                    directory,
                    probe_files,
                }
            }
        };
        let mut page = Vec::with_capacity(page_size.min(256));
        let mut complete = false;
        while page.len() < page_size {
            if let Some(entry) = state.buffered.pop_front() {
                if matches!(entry.name.as_str(), "." | "..") {
                    continue;
                }
                validate_entry_name(&entry.name)?;
                page.push(entry);
            } else {
                match self.runtime.block_on(state.reader.next_batch())? {
                    Some(batch) => state.buffered = batch.into(),
                    None => {
                        complete = true;
                        break;
                    }
                }
            }
        }
        // Only fallback probes need a pathname. Query it from the retained
        // handle, never from the caller's possibly renamed/replaced spelling.
        // File-only pages retain their ordinary QUERY_DIRECTORY fast path.
        let needs_probe = page.iter().any(|entry| {
            !entry.attributes.is_reparse_point() && (entry.is_directory || probe_files)
        });
        let probe_directory = if needs_probe {
            let (path, identity) = self
                .runtime
                .block_on(state.reader.current_path(&state.directory))?;
            self.verify_probe_directory(&path, identity)?;
            Some((path, identity))
        } else {
            None
        };
        let mut reparse = vec![false; page.len()];
        let mut probes = Vec::with_capacity(32);
        for (index, entry) in page.iter().enumerate() {
            if entry.attributes.is_reparse_point() {
                reparse[index] = true;
                continue;
            }
            if !entry.is_directory && !probe_files {
                continue;
            }
            let child = probe_directory
                .as_ref()
                .expect("probe directory queried")
                .0
                .join(&entry.name);
            let path = self.path_string(&child)?;
            let mut create = self.open_request(&path, libc::O_RDONLY)?;
            create.create_options = FILE_OPEN_REPARSE_POINT;
            create.desired_access = FileAccessMask::new(
                FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE,
            );
            probes.push((index, create));
            if probes.len() == 32 {
                self.probe_reparse_point_batch(&probes, &mut reparse)?;
                probes.clear();
            }
        }
        self.probe_reparse_point_batch(&probes, &mut reparse)?;
        if let Some((path, identity)) = probe_directory {
            self.verify_probe_directory(&path, identity)?;
        }
        let entries = page
            .into_iter()
            .zip(reparse)
            .map(|(mut entry, reparse)| {
                if reparse {
                    entry.attributes.0 |= FILE_ATTRIBUTE_REPARSE_POINT;
                }
                directory_attrs(&state.directory, entry, masks)
            })
            .collect::<VfResult<Vec<_>>>()?;
        if complete {
            self.runtime.block_on(state.reader.close())?;
        }
        let next = (!complete).then(|| DirPageCursor::new(state));
        Ok((entries, next))
    }

    pub(super) fn listdir_collect(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> VfResult<Vec<VfAttrs>> {
        let mut output = Vec::new();
        let mut pending = vec![Path::new("/").join(self.abs_path(dir))];
        while let Some(current) = pending.pop() {
            let mut cursor = None;
            let mut children = Vec::new();
            loop {
                let remaining = if max_count == 0 {
                    usize::MAX
                } else {
                    max_count.saturating_sub(output.len())
                };
                if remaining == 0 {
                    break;
                }
                let (entries, next) =
                    self.read_directory_page(&current, masks, cursor, remaining.min(256))?;
                for entry in &entries {
                    if recursive && entry.ftype == VfType::Directory {
                        children.push(
                            entry
                                .file
                                .path()
                                .expect("directory entry path")
                                .to_path_buf(),
                        );
                    }
                }
                output.extend(entries);
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
            pending.extend(children.into_iter().rev());
            if max_count != 0 && output.len() >= max_count {
                break;
            }
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_wire_flags_identify_file_and_directory_reparse_entries() {
        for is_directory in [false, true] {
            let mut entry = DirectoryEntry::default();
            entry.name = "link".into();
            entry.is_directory = is_directory;
            entry.attributes = smb2::types::flags::FileAttributes::new(
                FILE_ATTRIBUTE_REPARSE_POINT
                    | if is_directory {
                        smb2::types::flags::FileAttributes::DIRECTORY
                    } else {
                        0
                    },
            );
            let attrs = directory_attrs(Path::new("source"), entry, AttrMask::MODE).unwrap();
            assert_eq!(attrs.ftype, VfType::Symlink);
            assert_eq!(attrs.mode & libc::S_IFMT, libc::S_IFLNK);
            assert_eq!(attrs.file.path(), Some(Path::new("/source/link")));
        }
        let mut entry = DirectoryEntry::default();
        entry.name = "file".into();
        entry.index_number = Some(7);
        entry.size = 19;
        let attrs = directory_attrs(
            Path::new("source"),
            entry,
            AttrMask::MODE | AttrMask::SIZE | AttrMask::FILEID,
        )
        .unwrap();
        assert_eq!(attrs.ftype, VfType::Regular);
        assert_eq!((attrs.fileid, attrs.size), (7, 19));
        assert!(attrs.returned.contains(AttrMask::FILEID));
    }

    #[test]
    fn directory_names_cannot_replace_the_parent_or_introduce_components() {
        for name in [
            "",
            "/outside",
            "../sibling",
            "nested/child",
            "bad\0name",
            ".",
            "..",
        ] {
            let error = validate_entry_name(name).unwrap_err();
            assert_eq!(
                error.transport_kind(),
                Some(vfsi_core::TransportKind::InvalidReply),
                "invalid directory entry {name:?}",
            );
        }
    }

    #[test]
    fn directory_names_preserve_valid_unix_and_smb_mapped_characters() {
        for name in [
            "entry",
            "dots..",
            "é",
            "name\\with-backslash",
            "file:stream",
            "trailing ",
            "\u{f000}",
        ] {
            validate_entry_name(name).unwrap();
            let path = Path::new("parent").join(name);
            assert_eq!(path.parent(), Some(Path::new("parent")));
            assert_eq!(path.file_name().unwrap(), name);
        }
    }
}
