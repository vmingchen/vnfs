//! Owned, connection-scoped directory paging for application frontiers.
use super::{DirectoryListing, Error, ListDirOptions, Result, TraversalCompletion, Vfsi, VfsiExt};
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

/// Attach connection ownership while retaining one independent page outcome
/// per input. Invalid cursors are isolated before any native input dispatch.
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
    ) -> crate::VecResult<NativeDirectoryPage>,
) -> crate::VecResult<DirectoryPage> {
    use crate::{OpError, internal::vec_result_slots};
    if paths.len() > 32 || paths.len() != cursors.len() {
        return Err(Error::errno(None, libc::EINVAL as u32));
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    if options.recursive() {
        return Err(Error::errno(None, libc::EINVAL as u32));
    }
    let paths: Vec<_> = paths.iter().map(AsRef::as_ref).collect();
    let mut output: Vec<_> = (0..paths.len()).map(|_| None).collect();
    let mut pending = std::collections::VecDeque::new();
    for (index, (path, cursor)) in paths.iter().zip(cursors).enumerate() {
        match cursor {
            Some(cursor) if cursor.path != *path || !Arc::ptr_eq(&cursor.owner, owner) => {
                output[index] = Some(Err(OpError::not_attempted(Error::errno(
                    None,
                    libc::EBADF as u32,
                ))
                .with_context("vlistdir_pages", path)));
            }
            cursor => pending.push_back((index, cursor.map(|cursor| cursor.state))),
        }
    }
    let mut remaining_entries = options.max_entries_or(super::DEFAULT_DIRECTORY_MAX_ENTRIES);
    let mut remaining_bytes = options.max_path_bytes_or(super::DEFAULT_DIRECTORY_MAX_PATH_BYTES);
    while !pending.is_empty() {
        let candidates: Vec<_> = pending.iter().map(|(index, _)| paths[*index]).collect();
        let width = match capacity(&candidates) {
            Ok(width) => width.clamp(1, 32).min(pending.len()),
            Err(cause) => {
                // Capacity is a preflight policy query. Reject its first input
                // without blocking a different route behind that input.
                let (index, _) = pending.pop_front().unwrap();
                output[index] = Some(Err(
                    OpError::not_attempted(cause).with_context("vlistdir_pages", paths[index])
                ));
                continue;
            }
        };
        let batch: Vec<_> = pending.drain(..width).collect();
        let operands: Vec<_> = batch.iter().map(|(index, _)| paths[*index]).collect();
        let indices: Vec<_> = batch.iter().map(|(index, _)| *index).collect();
        let states = batch.into_iter().map(|(_, state)| state).collect();
        let size = page_size
            .get()
            .min(128)
            .min(remaining_entries.saturating_add(1));
        let pages = vec_result_slots(
            fetch(&operands, states, size, remaining_entries.saturating_add(1)),
            width,
        );
        for (index, result) in indices.into_iter().zip(pages) {
            let path = paths[index];
            let result = result
                .map_err(|error| {
                    if error.progress().is_some() {
                        OpError::unknown(
                            Error::invalid_reply(
                                None,
                                "directory page cannot report byte progress",
                            ),
                            None,
                        )
                    } else {
                        error
                    }
                })
                .and_then(|(listing, next, children)| {
                    let invalid =
                        |message| OpError::unknown(Error::invalid_reply(None, message), None);
                    if listing.path != path
                        || listing.entries.len() > size
                        || (listing.entries.is_empty() && next.is_some())
                        || listing
                            .entries
                            .iter()
                            .any(|entry| entry.path().parent() != Some(path))
                    {
                        return Err(invalid("invalid directory page parent or progress"));
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
                                return Err(invalid("invalid child directory seed"));
                            }
                        }
                    }
                    let bytes = listing.entries.iter().try_fold(0usize, |total, entry| {
                        total.checked_add(entry.path().as_os_str().len())
                    });
                    let Some(bytes) = bytes.filter(|&bytes| bytes <= remaining_bytes) else {
                        return Err(OpError::failed(
                            Error::errno(None, libc::EFBIG as u32),
                            None,
                        ));
                    };
                    if listing.entries.len() > remaining_entries {
                        return Err(OpError::failed(
                            Error::errno(None, libc::EFBIG as u32),
                            None,
                        ));
                    }
                    // Charge only complete returned pages, atomically; rejecting a
                    // page does not consume a sibling's returned-data allowance.
                    remaining_entries -= listing.entries.len();
                    remaining_bytes -= bytes;
                    let seed = |path, state| DirectoryCursor {
                        path,
                        state,
                        owner: owner.clone(),
                    };
                    Ok(DirectoryPage {
                        continuation: next.map(|state| seed(path.to_owned(), state)),
                        children: children
                            .into_iter()
                            .map(|(path, state)| seed(path, state))
                            .collect(),
                        listing,
                    })
                })
                .map_err(|error| error.with_context("vlistdir_pages", path));
            output[index] = Some(result);
        }
    }
    Ok(output
        .into_iter()
        .map(|slot| slot.expect("all page inputs assigned"))
        .collect())
}

pub(super) fn fallback_pages<F: Vfsi + ?Sized, P: AsRef<Path>>(
    fs: &F,
    paths: &[P],
    cursors: Vec<Option<DirectoryCursor>>,
    options: ListDirOptions,
) -> crate::VecResult<DirectoryPage> {
    use crate::{OpError, internal::vec_result_slots};
    if paths.len() > 32 || paths.len() != cursors.len() {
        return Err(Error::errno(None, libc::EINVAL as u32));
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    if options.recursive() {
        return Err(Error::errno(None, libc::EINVAL as u32));
    }
    let paths: Vec<_> = paths.iter().map(AsRef::as_ref).collect();
    let mut output: Vec<_> = (0..paths.len()).map(|_| None).collect();
    let mut indices = Vec::new();
    for (index, cursor) in cursors.into_iter().enumerate() {
        if cursor.is_some() {
            output[index] = Some(Err(OpError::not_attempted(Error::errno(
                None,
                libc::EBADF as u32,
            ))
            .with_context("vlistdir_pages", paths[index])));
        } else {
            indices.push(index);
        }
    }
    if indices.is_empty() {
        return Ok(output.into_iter().map(Option::unwrap).collect());
    }
    let pending: Vec<_> = indices.iter().map(|&index| paths[index]).collect();
    let mut pages: Vec<_> = pending
        .iter()
        .map(|path| DirectoryPage {
            listing: DirectoryListing {
                path: path.to_path_buf(),
                entries: Vec::new(),
            },
            continuation: None,
            children: Vec::new(),
        })
        .collect();
    let mut errors: Vec<Option<OpError>> = (0..pending.len()).map(|_| None).collect();
    let mut invalid_index = false;
    let options = options.resolve(fs.limits());
    let mut remaining_entries = options.max_entries().expect("resolved");
    let mut remaining_bytes = options.max_path_bytes().expect("resolved");
    let mut delivered = false;
    let result = fs.vlistdirs(&pending, options, |index, mut listing| {
        delivered = true;
        let Some(page) = pages.get_mut(index) else {
            invalid_index = true;
            return Err(Error::invalid_reply(None, "invalid directory page index"));
        };
        let valid = (|| -> Result<usize> {
            if page.listing.path != listing.path
                || listing
                    .entries
                    .iter()
                    .any(|entry| entry.path().parent() != Some(listing.path.as_path()))
            {
                return Err(Error::invalid_reply(None, "invalid directory page parent"));
            }
            let bytes = listing
                .entries
                .iter()
                .try_fold(0usize, |total, entry| {
                    total.checked_add(entry.path().as_os_str().len())
                })
                .ok_or_else(|| Error::errno(None, libc::EFBIG as u32))?;
            if listing.entries.len() > remaining_entries || bytes > remaining_bytes {
                return Err(Error::errno(None, libc::EFBIG as u32));
            }
            Ok(bytes)
        })();
        match valid {
            Ok(bytes) => {
                remaining_entries -= listing.entries.len();
                remaining_bytes -= bytes;
                page.listing.entries.append(&mut listing.entries);
                Ok(std::ops::ControlFlow::Continue(()))
            }
            Err(cause) => {
                errors[index] = Some(OpError::failed(cause.clone(), None));
                Err(cause)
            }
        }
    });
    let results = match result {
        Err(cause) if delivered => (0..pending.len())
            .map(|_| Err(OpError::unknown(cause.clone(), None)))
            .collect(),
        result => vec_result_slots(result, pending.len()),
    };
    for (((index, result), page), own_error) in
        indices.into_iter().zip(results).zip(pages).zip(errors)
    {
        let result = if invalid_index {
            Err(OpError::unknown(
                Error::invalid_reply(None, "invalid directory page index"),
                None,
            ))
        } else if result
            .as_ref()
            .is_err_and(|error| error.progress().is_some())
        {
            Err(OpError::unknown(
                Error::invalid_reply(None, "directory completion contains byte progress"),
                None,
            ))
        } else if let Some(error) = own_error {
            Err(error)
        } else {
            result.and_then(|completion| {
                if completion == TraversalCompletion::Complete {
                    Ok(page)
                } else {
                    Err(OpError::unknown(
                        Error::invalid_reply(None, "incomplete fallback page"),
                        None,
                    ))
                }
            })
        };
        output[index] =
            Some(result.map_err(|error| error.with_context("vlistdir_pages", paths[index])));
    }
    Ok(output.into_iter().map(Option::unwrap).collect())
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
        let mut pages = fs.dir_pages_all(
            &[path],
            vec![cursor],
            options,
            NonZeroUsize::new(128).expect("nonzero page size"),
        )?;
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
                    .map(Ok)
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
    fn quota_failure_keeps_prior_pages_and_allows_later_empty_roots() {
        let owner = Arc::new(());
        let paths = ["/a", "/b", "/c"];
        let mut fetched = Vec::new();
        let results = native_directory_pages(
            &owner,
            &paths,
            (0..paths.len()).map(|_| None).collect(),
            ListDirOptions::new().with_max_entries(1),
            NonZeroUsize::new(128).unwrap(),
            |_| Ok(1),
            |paths, _, size, _| {
                fetched.push(paths[0].to_path_buf());
                assert!(size <= 2);
                Ok(vec![Ok((
                    DirectoryListing {
                        path: paths[0].to_path_buf(),
                        entries: if paths[0] == Path::new("/c") {
                            vec![]
                        } else {
                            vec![entry(paths[0].join("child").to_str().unwrap())]
                        },
                    },
                    None,
                    vec![],
                ))])
            },
        )
        .unwrap();
        assert_eq!(
            fetched,
            [
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c")
            ]
        );
        assert!(results[0].is_ok() && results[2].is_ok());
        let error = results[1].as_ref().unwrap_err();
        assert_eq!(error.cause().index(), None);
        assert_eq!(error.cause().raw_code(), libc::EFBIG as u32);
    }

    #[test]
    fn foreign_or_retargeted_cursors_are_isolated_before_fetch() {
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
            let results = native_directory_pages(
                actual_owner,
                &[Path::new("/missing"), Path::new(path)],
                vec![None, Some(cursor)],
                ListDirOptions::new(),
                NonZeroUsize::new(1).unwrap(),
                |paths| {
                    assert_eq!(paths, &[Path::new("/missing")]);
                    Ok(32)
                },
                |paths, cursors, _, _| {
                    assert_eq!(paths, &[Path::new("/missing")]);
                    assert!(cursors[0].is_none());
                    Ok(vec![Ok((
                        DirectoryListing {
                            path: paths[0].to_owned(),
                            entries: vec![],
                        },
                        None,
                        vec![],
                    ))])
                },
            )
            .unwrap();
            assert!(results[0].is_ok());
            let error = results[1].as_ref().unwrap_err();
            assert_eq!(error.state(), crate::FailureState::NotAttempted);
            assert_eq!(error.cause().index(), None);
            assert_eq!(error.cause().raw_code(), libc::EBADF as u32);
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
                Ok(vec![Ok((
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
                ))])
            },
        )
        .unwrap()
        .remove(0)
        .unwrap_err();
        assert_eq!(error.cause().raw_code(), libc::EFBIG as u32);
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
                    Ok(vec![Ok(match invalid {
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
                    })])
                },
            )
            .unwrap()
            .remove(0)
            .unwrap_err();
            assert_eq!(
                error.cause().transport_kind(),
                Some(crate::TransportKind::InvalidReply)
            );
        }
    }
}
