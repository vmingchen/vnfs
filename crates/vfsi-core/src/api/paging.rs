//! Owned, connection-scoped directory paging for application frontiers.
use super::{DirectoryListing, Error, ListDirOptions, Result, TraversalCompletion, Vfsi};
use crate::{VfError, VfResult};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Backend-owned continuation state for a paged directory visit.
///
/// The state is dropped automatically when the caller stops or an error
/// occurs, so backends do not need an explicit cursor-close operation.
#[doc(hidden)]
pub struct NativeDirectoryCursor(Box<dyn std::any::Any + Send>);

impl NativeDirectoryCursor {
    pub fn new<T: std::any::Any + Send>(state: T) -> Self {
        Self(Box::new(state))
    }

    pub fn is<T: std::any::Any + Send>(&self) -> bool {
        self.0.is::<T>()
    }

    pub fn into_state<T: std::any::Any + Send>(self) -> VfResult<T> {
        self.0
            .downcast::<T>()
            .map(|state| *state)
            .map_err(|_| VfError::from_kind(0, std::io::ErrorKind::InvalidInput))
    }
}

/// An owned continuation or child seed from this client's directory pages.
///
/// Continuations retain the opened directory's identity when supported. Child
/// seeds resolve a name relative to its observed parent; replacing that name
/// can change the child opened. Paths identify the original operand and do not
/// redirect an anchored seed.
/// Seeds cannot be cloned, must stay with their originating client, and release
/// their retained resources on drop. They do not provide a filesystem snapshot.
pub struct DirectoryCursor {
    path: PathBuf,
    state: NativeDirectoryCursor,
    owner: Arc<()>,
}

impl DirectoryCursor {
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl std::fmt::Debug for DirectoryCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectoryCursor")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// One owned directory page. None continuation means EOF for this directory.
/// Children are optional anchored seeds for directory entries in this page.
/// Retain only admitted children; dropping the rest performs no enumeration.
#[derive(Debug)]
pub struct DirectoryPage {
    pub listing: DirectoryListing,
    pub continuation: Option<DirectoryCursor>,
    pub children: Vec<DirectoryCursor>,
}

/// Adapter projection before applying public cursor ownership.
#[doc(hidden)]
pub type NativeDirectoryPage = (
    DirectoryListing,
    Option<NativeDirectoryCursor>,
    Vec<(PathBuf, NativeDirectoryCursor)>,
);

/// Validate every cursor before fetching and attach connection ownership to all
/// returned native states. Backend adapters keep their implementation types private.
#[doc(hidden)]
pub fn native_directory_pages<P: AsRef<Path>>(
    owner: &Arc<()>,
    paths: &[P],
    cursors: Vec<Option<DirectoryCursor>>,
    options: ListDirOptions,
    page_size: NonZeroUsize,
    mut capacity: impl FnMut(&[&Path]) -> Result<usize>,
    mut fetch: impl FnMut(
        &[&Path],
        Vec<Option<NativeDirectoryCursor>>,
        usize,
        usize,
    ) -> Result<Vec<NativeDirectoryPage>>,
) -> Result<Vec<DirectoryPage>> {
    if options.recursive() || paths.len() > 32 || paths.len() != cursors.len() {
        return Err(Error::errno(0, libc::EINVAL as u32));
    }
    for (index, (path, cursor)) in paths.iter().zip(&cursors).enumerate() {
        if let Some(cursor) = cursor
            && (cursor.path != path.as_ref() || !Arc::ptr_eq(&cursor.owner, owner))
        {
            return Err(Error::errno(index, libc::EBADF as u32)
                .with_context("vlistdir_pages", path.as_ref()));
        }
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let paths: Vec<_> = paths.iter().map(AsRef::as_ref).collect();
    let mut remaining_entries = options.max_entries_or(super::DEFAULT_DIRECTORY_MAX_ENTRIES);
    let mut remaining_bytes = options.max_path_bytes_or(super::DEFAULT_DIRECTORY_MAX_PATH_BYTES);
    let mut states: std::collections::VecDeque<_> = cursors
        .into_iter()
        .map(|cursor| cursor.map(|c| c.state))
        .collect();
    let mut output = Vec::with_capacity(paths.len());
    while output.len() < paths.len() {
        let start = output.len();
        let width = capacity(&paths[start..])?
            .clamp(1, 32)
            .min(paths.len() - start);
        let operands = &paths[start..start + width];
        let size = page_size
            .get()
            .min(128)
            .min(remaining_entries.saturating_add(1));
        let states = states.drain(..width).collect();
        let pages = fetch(operands, states, size, remaining_entries.saturating_add(1)).map_err(
            |error| match error.index() {
                Some(index) if index < width => error.with_index(start + index),
                Some(_) => Error::invalid_reply(None, "invalid directory page error index"),
                None => error,
            },
        )?;
        if pages.len() != width {
            return Err(Error::invalid_reply(None, "invalid directory page count"));
        }
        for (offset, ((listing, next, children), path)) in
            pages.into_iter().zip(operands).enumerate()
        {
            let index = start + offset;
            if listing.path != *path
                || listing.entries.len() > size
                || (listing.entries.is_empty() && next.is_some())
            {
                return Err(Error::invalid_reply(
                    index,
                    "invalid directory page progress",
                ));
            }
            remaining_entries = remaining_entries
                .checked_sub(listing.entries.len())
                .ok_or_else(|| {
                    Error::errno(index, libc::EFBIG as u32).with_context("vlistdir_pages", path)
                })?;
            for entry in &listing.entries {
                remaining_bytes = remaining_bytes
                    .checked_sub(entry.path().as_os_str().len())
                    .ok_or_else(|| {
                        Error::errno(index, libc::EFBIG as u32)
                            .with_context("vlistdir_pages", entry.path())
                    })?;
            }
            if !children.is_empty() {
                let mut directories: std::collections::HashSet<_> = listing
                    .entries
                    .iter()
                    .filter(|entry| entry.attrs().is_dir())
                    .map(|entry| entry.path())
                    .collect();
                for (child, _) in &children {
                    if !directories.remove(child.as_path()) {
                        return Err(Error::invalid_reply(index, "invalid child directory seed"));
                    }
                }
            }
            let seed = |path: PathBuf, state| DirectoryCursor {
                path,
                state,
                owner: owner.clone(),
            };
            output.push(DirectoryPage {
                continuation: next.map(|state| seed(path.to_path_buf(), state)),
                children: children
                    .into_iter()
                    .map(|(path, state)| seed(path, state))
                    .collect(),
                listing,
            });
        }
    }
    Ok(output)
}

pub(super) fn fallback_pages<F: Vfsi + ?Sized, P: AsRef<Path>>(
    fs: &F,
    paths: &[P],
    cursors: Vec<Option<DirectoryCursor>>,
    options: ListDirOptions,
) -> Result<Vec<DirectoryPage>> {
    if options.recursive() || paths.len() > 32 || paths.len() != cursors.len() {
        return Err(Error::errno(0, libc::EINVAL as u32));
    }
    if let Some(index) = cursors.iter().position(Option::is_some) {
        return Err(Error::errno(index, libc::EBADF as u32));
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut pages: Vec<_> = paths
        .iter()
        .map(|path| DirectoryPage {
            listing: DirectoryListing {
                path: path.as_ref().to_path_buf(),
                entries: Vec::new(),
            },
            continuation: None,
            children: Vec::new(),
        })
        .collect();
    let options = options.resolve(fs.limits());
    let mut remaining_entries = options.max_entries().expect("resolved");
    let mut remaining_bytes = options.max_path_bytes().expect("resolved");
    let completions = fs.vlistdirs(paths, options, |index, mut listing| {
        let page = pages
            .get_mut(index)
            .ok_or_else(|| Error::invalid_reply(index, "invalid directory page index"))?;
        if page.listing.path != listing.path {
            return Err(Error::invalid_reply(index, "invalid directory page parent"));
        }
        remaining_entries = remaining_entries
            .checked_sub(listing.entries.len())
            .ok_or_else(|| Error::errno(index, libc::EFBIG as u32))?;
        for entry in &listing.entries {
            remaining_bytes = remaining_bytes
                .checked_sub(entry.path().as_os_str().len())
                .ok_or_else(|| Error::errno(index, libc::EFBIG as u32))?;
        }
        page.listing.entries.append(&mut listing.entries);
        Ok(std::ops::ControlFlow::Continue(()))
    })?;
    if completions.len() != paths.len()
        || completions
            .iter()
            .any(|c| *c != TraversalCompletion::Complete)
    {
        return Err(Error::invalid_reply(
            None,
            "invalid directory page completion",
        ));
    }
    Ok(pages)
}

/// Collect a bounded directory while preserving child seeds and continuations.
pub(super) fn collect_directory<F: Vfsi + ?Sized>(
    fs: &F,
    path: &Path,
    cursor: Option<DirectoryCursor>,
    options: ListDirOptions,
) -> Result<(Vec<super::DirEntry>, Vec<DirectoryCursor>)> {
    let mut options = options.resolve(fs.limits());
    let mut cursor = cursor;
    let mut entries = Vec::new();
    let mut children = Vec::new();
    loop {
        let mut pages = fs.vlistdir_pages(
            &[path],
            vec![cursor],
            options,
            NonZeroUsize::new(128).expect("nonzero page size"),
        )?;
        if pages.len() != 1 {
            return Err(Error::invalid_reply(None, "invalid directory page count"));
        }
        let mut page = pages.pop().expect("validated page count");
        if page.listing.path != path
            || (page.listing.entries.is_empty() && page.continuation.is_some())
        {
            return Err(Error::invalid_reply(
                None,
                "invalid directory page progress",
            ));
        }
        let remaining_entries = options
            .max_entries()
            .expect("resolved")
            .checked_sub(page.listing.entries.len())
            .ok_or_else(|| Error::errno(0, libc::EFBIG as u32).with_context("listdir", path))?;
        let remaining_bytes = page
            .listing
            .entries
            .iter()
            .try_fold(
                options.max_path_bytes().expect("resolved"),
                |bytes, entry| bytes.checked_sub(entry.path().as_os_str().len()),
            )
            .ok_or_else(|| Error::errno(0, libc::EFBIG as u32).with_context("listdir", path))?;
        options = options
            .with_max_entries(remaining_entries)
            .with_max_path_bytes(remaining_bytes);
        entries.append(&mut page.listing.entries);
        children.append(&mut page.children);
        cursor = page.continuation;
        if cursor.is_none() {
            return Ok((entries, children));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountDrop(Arc<AtomicUsize>);
    impl Drop for CountDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn entry(path: &str) -> super::super::DirEntry {
        super::super::DirEntry::new(
            path.into(),
            crate::metadata_from_attrs(crate::VfAttrs {
                file: crate::FileOperand::from_path(path),
                masks: crate::AttrMask::MODE,
                ftype: crate::VfType::Directory,
                ..Default::default()
            }),
        )
    }

    #[test]
    fn empty_pages_use_negotiated_waves_and_a_single_entry_probe() {
        let owner = Arc::new(());
        let paths = ["/a", "/b", "/c", "/d", "/e"];
        let mut waves = Vec::new();
        let pages = native_directory_pages(
            &owner,
            &paths,
            (0..paths.len()).map(|_| None).collect(),
            ListDirOptions::new()
                .with_max_entries(0)
                .with_max_path_bytes(0),
            NonZeroUsize::new(128).unwrap(),
            |_| Ok(2),
            |paths, _, size, probe| {
                assert_eq!((size, probe), (1, 1));
                waves.push(paths.len());
                Ok(paths
                    .iter()
                    .map(|path| {
                        (
                            DirectoryListing {
                                path: path.to_path_buf(),
                                entries: vec![],
                            },
                            None,
                            vec![],
                        )
                    })
                    .collect())
            },
        )
        .unwrap();
        assert_eq!(waves, [2, 2, 1]);
        assert_eq!(pages.len(), 5);
    }

    #[test]
    fn oversized_frontier_is_rejected_before_capacity_or_io() {
        let owner = Arc::new(());
        let paths = ["/"; 33];
        let error = native_directory_pages(
            &owner,
            &paths,
            (0..paths.len()).map(|_| None).collect(),
            ListDirOptions::new(),
            NonZeroUsize::new(128).unwrap(),
            |_| panic!("oversized cohort must fail before route discovery"),
            |_, _, _, _| panic!("oversized cohort must fail before I/O"),
        )
        .unwrap_err();
        assert_eq!(error.raw_code(), libc::EINVAL as u32);
    }

    #[test]
    fn quota_failure_stops_before_later_waves_and_rebases_the_index() {
        let owner = Arc::new(());
        let paths = ["/a", "/b", "/c"];
        let mut fetched = Vec::new();
        let error = native_directory_pages(
            &owner,
            &paths,
            (0..paths.len()).map(|_| None).collect(),
            ListDirOptions::new().with_max_entries(1),
            NonZeroUsize::new(128).unwrap(),
            |_| Ok(1),
            |paths, _, size, _| {
                fetched.push(paths[0].to_path_buf());
                assert!(size <= 2);
                Ok(vec![(
                    DirectoryListing {
                        path: paths[0].to_path_buf(),
                        entries: vec![entry(paths[0].join("child").to_str().unwrap())],
                    },
                    None,
                    vec![],
                )])
            },
        )
        .unwrap_err();
        assert_eq!(fetched, [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(error.index(), Some(1));
        assert_eq!(error.raw_code(), libc::EFBIG as u32);
    }

    #[test]
    fn foreign_or_retargeted_cursors_preflight_before_any_fetch() {
        fn send<T: Send>() {}
        send::<DirectoryCursor>();
        let owner = Arc::new(());
        let other = Arc::new(());
        for (actual_owner, path) in [(&other, "/original"), (&owner, "/different")] {
            let cursor = DirectoryCursor {
                path: "/original".into(),
                state: NativeDirectoryCursor::new(()),
                owner: owner.clone(),
            };
            let error = native_directory_pages(
                actual_owner,
                &[Path::new("/missing"), Path::new(path)],
                vec![None, Some(cursor)],
                ListDirOptions::new(),
                NonZeroUsize::new(1).unwrap(),
                |_| panic!("must validate before capacity lookup"),
                |_, _, _, _| panic!("must validate every cursor before any I/O"),
            )
            .unwrap_err();
            assert_eq!(error.index(), Some(1));
            assert_eq!(error.raw_code(), libc::EBADF as u32);
        }
    }

    #[test]
    fn quota_failure_drops_all_undelivered_child_states() {
        let owner = Arc::new(());
        let drops = Arc::new(AtomicUsize::new(0));
        let error = native_directory_pages(
            &owner,
            &[Path::new("/")],
            vec![None],
            ListDirOptions::new().with_max_entries(1),
            NonZeroUsize::new(128).unwrap(),
            |_| Ok(32),
            |_, _, _, _| {
                Ok(vec![(
                    DirectoryListing {
                        path: "/".into(),
                        entries: vec![entry("/a"), entry("/b")],
                    },
                    None,
                    vec![
                        (
                            "/a".into(),
                            NativeDirectoryCursor::new(CountDrop(drops.clone())),
                        ),
                        (
                            "/b".into(),
                            NativeDirectoryCursor::new(CountDrop(drops.clone())),
                        ),
                    ],
                )])
            },
        )
        .unwrap_err();
        assert_eq!(error.raw_code(), libc::EFBIG as u32);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn invalid_progress_and_child_projection_are_rejected() {
        let owner = Arc::new(());
        for invalid in 0..3 {
            let error = native_directory_pages(
                &owner,
                &[Path::new("/")],
                vec![None],
                ListDirOptions::new(),
                NonZeroUsize::new(128).unwrap(),
                |_| Ok(32),
                |_, _, _, _| {
                    Ok(vec![match invalid {
                        0 => (
                            DirectoryListing {
                                path: "/".into(),
                                entries: vec![],
                            },
                            Some(NativeDirectoryCursor::new(())),
                            vec![],
                        ),
                        1 => (
                            DirectoryListing {
                                path: "/".into(),
                                entries: vec![entry("/a")],
                            },
                            None,
                            vec![("/unobserved".into(), NativeDirectoryCursor::new(()))],
                        ),
                        _ => (
                            DirectoryListing {
                                path: "/".into(),
                                entries: vec![entry("/a")],
                            },
                            None,
                            vec![
                                ("/a".into(), NativeDirectoryCursor::new(())),
                                ("/a".into(), NativeDirectoryCursor::new(())),
                            ],
                        ),
                    }])
                },
            )
            .unwrap_err();
            assert_eq!(
                error.transport_kind(),
                Some(crate::TransportKind::InvalidReply)
            );
        }
    }
}
