use super::*;

fn validate_entry_name(name: &str) -> VfRes {
    if name.is_empty() || matches!(name, "." | "..") || name.bytes().any(|b| b == b'/' || b == 0) {
        return Err(VfError::invalid_reply(
            None,
            "invalid SMB directory entry name",
        ));
    }
    Ok(())
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
        let tree_id = self.tree.tree_id;
        let jobs = requests
            .iter()
            .map(|(_, create)| probe_reparse_point(connection.clone(), tree_id, create.clone()));
        let outcomes = self.runtime.block_on(join_all(jobs));
        queue_deferred_closes(&outcomes, &mut self.deferred_closes);
        for ((index, _), outcome) in requests.iter().zip(outcomes) {
            reparse_points[*index] = outcome.result?;
        }
        Ok(())
    }

    pub(super) fn listdir_rec(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
        probe_file_reparse_points: bool,
        output: &mut Vec<VfAttrs>,
    ) -> VfResult<()> {
        let mut pending = vec![self.abs_path(dir)];
        while let Some(current) = pending.pop() {
            if max_count != 0 && output.len() >= max_count {
                break;
            }
            let path = self.path_string(&current)?;
            let entries = self
                .runtime
                .block_on(self.client.list_directory(&mut self.tree, &path))
                .map_err(|error| smb_error(error, 0))?;
            let mut reparse_points = vec![false; entries.len()];
            let mut requests = Vec::with_capacity(SMB_REPARSE_PROBE_CONCURRENCY);
            let mut remaining = if max_count == 0 {
                usize::MAX
            } else {
                max_count.saturating_sub(output.len())
            };
            for (index, entry) in entries.iter().enumerate() {
                if matches!(entry.name.as_str(), "." | "..") {
                    continue;
                }
                if remaining == 0 {
                    break;
                }
                remaining -= 1;
                validate_entry_name(&entry.name)?;
                if !entry.is_directory && !probe_file_reparse_points {
                    continue;
                }
                let child = current.join(&entry.name);
                let child_path = self.path_string(&child)?;
                let mut create = self.open_request(&child_path, libc::O_RDONLY)?;
                // Leave the object type unconstrained: some SMB servers
                // expose a directory symlink as a reparse object that is
                // not itself a directory.
                create.create_options = FILE_OPEN_REPARSE_POINT;
                create.desired_access = FileAccessMask::new(
                    FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE,
                );
                requests.push((index, create));
                if requests.len() == SMB_REPARSE_PROBE_CONCURRENCY {
                    self.probe_reparse_point_batch(&requests, &mut reparse_points)?;
                    requests.clear();
                }
            }
            self.probe_reparse_point_batch(&requests, &mut reparse_points)?;
            let mut children = Vec::new();
            for (index, entry) in entries.into_iter().enumerate() {
                if matches!(entry.name.as_str(), "." | "..") {
                    continue;
                }
                if max_count != 0 && output.len() >= max_count {
                    break;
                }
                let child = current.join(&entry.name);
                // SMB directory entries discard FILE_ATTRIBUTE_REPARSE_POINT;
                // the bounded probes above open candidate children without following them.
                let is_reparse_point = reparse_points[index];
                let mut attrs = VfAttrs {
                    file: FileOperand::Path {
                        base: VfPathBase::Abs,
                        path: Path::new("/").join(&child),
                    },
                    masks,
                    ftype: if is_reparse_point {
                        VfType::Symlink
                    } else if entry.is_directory {
                        VfType::Directory
                    } else {
                        VfType::Regular
                    },
                    ..VfAttrs::default()
                };
                attrs.returned = AttrMask::empty();
                if masks.contains(AttrMask::MODE) {
                    attrs.mode = if is_reparse_point {
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
                if recursive && entry.is_directory && !is_reparse_point {
                    children.push(child);
                }
                output.push(attrs);
            }
            for child in children.into_iter().rev() {
                pending.push(child);
            }
        }
        Ok(())
    }

    pub(super) fn listdir_with_reparse_policy(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
        probe_file_reparse_points: bool,
    ) -> VfResult<Vec<VfAttrs>> {
        let mut output = Vec::new();
        self.listdir_rec(
            dir,
            masks,
            max_count,
            recursive,
            probe_file_reparse_points,
            &mut output,
        )?;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
