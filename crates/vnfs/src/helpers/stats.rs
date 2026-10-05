use crate::{Attributes, AttrsOptions, FileType, ListDirOptions, Result, Vfsi};
use std::path::Path;

/// Logical sizes, not allocated disk space. Symlink targets are never followed.
/// Directories include the root. Hard-linked files are counted once per name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeStats {
    pub files: u64,
    pub directories: u64,
    pub symlinks: u64,
    pub other: u64,
    pub file_bytes: u64,
}
impl TreeStats {
    fn add(&mut self, metadata: &crate::Attrs) -> Result<()> {
        let count = match metadata.file_type() {
            FileType::Regular => {
                self.file_bytes = self
                    .file_bytes
                    .checked_add(metadata.len())
                    .ok_or_else(overflow)?;
                &mut self.files
            }
            FileType::Directory => &mut self.directories,
            FileType::Symlink => &mut self.symlinks,
            _ => &mut self.other,
        };
        *count = count.checked_add(1).ok_or_else(overflow)?;
        Ok(())
    }
}
fn overflow() -> crate::Error {
    crate::Error::client(0, libc::EOVERFLOW as u32)
}
/// Fold bounded directory pages without collecting a tree or issuing per-entry stat.
/// `options` supplies depth/entry/path budgets; recursive visiting is enabled.
/// Explicit depth truncation produces statistics for only the visited portion.
/// Concurrent changes can affect the result; this is not a snapshot.
pub fn tree_stats(
    fs: &impl Vfsi,
    root: impl AsRef<Path>,
    options: ListDirOptions,
) -> Result<TreeStats> {
    let root = root.as_ref();
    let fields = Attributes::MODE | Attributes::SIZE;
    let mut metadata = fs.vgetattrs(
        &[root],
        AttrsOptions::new().fields(fields).follow_symlinks(false),
    )?;
    if metadata.len() != 1 {
        return Err(crate::Error::transport_with_kind(
            None,
            crate::TransportKind::InvalidReply,
            "tree_stats: invalid metadata count",
        ));
    }
    let metadata = metadata.remove(0);
    let mut stats = TreeStats::default();
    stats.add(&metadata)?;
    if metadata.is_dir() {
        let completion = fs.vlistdirs(
            &[root],
            options.recursive(true).fields(fields),
            |index, page| {
                if index != 0 {
                    return Err(crate::Error::transport_with_kind(
                        None,
                        crate::TransportKind::InvalidReply,
                        "tree_stats: invalid root index",
                    ));
                }
                for entry in page.entries {
                    stats.add(entry.attrs())?;
                }
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )?;
        if completion.len() != 1 || completion[0] != crate::TraversalCompletion::Complete {
            return Err(crate::Error::transport_with_kind(
                None,
                crate::TransportKind::InvalidReply,
                "tree_stats: incomplete traversal",
            ));
        }
    }
    Ok(stats)
}
