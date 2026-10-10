//! Native execution of the portable VFSI contract.
use std::path::Path;
use vfsi_core::api::*;
#[doc(hidden)]
pub fn vector_index(error: vfsi_core::api::Error, index: usize) -> vfsi_core::api::Error {
    if error.index().is_some() {
        error.with_index(index)
    } else {
        error
    }
}
struct VectorBudget {
    entries: usize,
    bytes: usize,
}
impl VectorBudget {
    fn new(entries: usize, bytes: usize) -> Self {
        Self { entries, bytes }
    }
    fn charge(&mut self, path: &Path) -> Result<()> {
        if self.entries == 0 {
            return Err(vfsi_core::api::Error::client(0, libc::EFBIG as u32)
                .with_context("vector traversal", path));
        }
        self.charge_path(path)?;
        self.entries -= 1;
        Ok(())
    }
    fn charge_path(&mut self, path: &Path) -> Result<()> {
        let bytes = path.as_os_str().len();
        if bytes > self.bytes {
            return Err(vfsi_core::api::Error::client(0, libc::EFBIG as u32)
                .with_context("vector traversal", path));
        }
        self.bytes -= bytes;
        Ok(())
    }
}
pub fn visit_directory_pages<P: AsRef<Path>>(
    roots: &[P],
    policy: vfsi_core::api::ListDirOptions,
    limits: ResourceLimits,
    mut capacity: impl FnMut(&[&Path]) -> Result<usize>,
    mut validate_root: impl FnMut(&Path) -> Result<()>,
    mut fetch: impl FnMut(
        &[&Path],
        Vec<Option<crate::DirPageCursor>>,
        usize,
        usize,
    ) -> Result<Vec<crate::DirectoryPage>>,
    mut callback: impl FnMut(usize, DirectoryListing) -> Result<std::ops::ControlFlow<()>>,
) -> Result<Vec<crate::TraversalCompletion>> {
    let directory = policy.directory_options(limits);
    let walk = policy.walk_options(limits);
    let mut budget = VectorBudget::new(directory.entry_limit(), directory.path_byte_limit());
    let mut completed = Vec::new();
    let mut pending = std::collections::VecDeque::new();
    for (index, root) in roots.iter().enumerate() {
        pending.push_back((index, root.as_ref().to_path_buf(), 0usize, None));
    }
    while !pending.is_empty() {
        let owner = pending.front().unwrap().0;
        if policy.is_recursive() && pending.front().unwrap().2 == 0 {
            validate_root(&pending.front().unwrap().1)
                .map_err(|error| vector_index(error, owner))?;
        }
        // Recursive traversal completes one input root before starting another.
        let candidate_paths: Vec<_> = pending
            .iter()
            .take_while(|state| !policy.is_recursive() || state.0 == owner)
            .take(32)
            .map(|state| state.1.as_path())
            .collect();
        let capacity = capacity(&candidate_paths)?.clamp(1, 32);
        let cohort_size = if policy.is_recursive() {
            pending
                .iter()
                .take(capacity)
                .take_while(|state| state.0 == owner)
                .count()
        } else {
            pending.len().min(capacity)
        };
        let mut cohort: Vec<_> = (0..cohort_size)
            .map(|_| pending.pop_front().unwrap())
            .collect();
        let cursors = cohort.iter_mut().map(|state| state.3.take()).collect();
        let paths: Vec<_> = cohort
            .iter()
            .map(|(_, path, _, _)| path.as_path())
            .collect();
        let requested = budget.entries.saturating_add(1);
        let mut deferred_error = None;
        let pages = match fetch(&paths, cursors, 1, requested) {
            Ok(pages) => pages,
            Err(error)
                if !policy.is_recursive()
                    && !error.is_transport()
                    && error.index().is_some_and(|i| i > 0 && i < cohort.len()) =>
            {
                // A speculative later root must not hide an earlier callback's
                // cancellation. Re-fetch only the known successful read-only
                // prefix; this exceptional path never replays a mutation.
                let failed = error.index().unwrap();
                deferred_error = Some(vector_index(error, cohort[failed].0));
                let prefix = fetch(
                    &paths[..failed],
                    (0..failed).map(|_| None).collect(),
                    1,
                    requested,
                )
                .map_err(|error| error.map_index(|i| cohort.get(i).map_or(i, |state| state.0)))?;
                cohort.truncate(failed);
                prefix
            }
            Err(error) => return Err(error.map_index(|i| cohort.get(i).map_or(i, |state| state.0))),
        };
        if pages.len() != cohort.len() {
            return Err(vfsi_core::api::Error::transport(
                None,
                "invalid directory page result count",
            ));
        }
        let mut children = Vec::new();
        let mut wave: Vec<_> = cohort
            .into_iter()
            .map(|(index, path, depth, _)| (index, path, depth, true))
            .zip(pages)
            .collect();
        while !wave.is_empty() {
            let mut continuations = Vec::new();
            for ((index, path, depth, first), (mut page, next, seeds)) in wave {
                if first && policy.is_recursive() {
                    budget
                        .charge_path(&path)
                        .map_err(|error| vector_index(error, index))?;
                }
                if page.path != path || (page.entries.is_empty() && next.is_some()) {
                    return Err(vfsi_core::api::Error::transport(
                        Some(index),
                        "invalid directory page parent or progress",
                    ));
                }
                let mut seeds: std::collections::HashMap<_, _> = seeds.into_iter().collect();
                let mut page_error = None;
                let mut accepted = 0;
                for entry in &page.entries {
                    if let Err(error) = budget.charge(entry.path()) {
                        page_error = Some(vector_index(error, index));
                        break;
                    }
                    accepted += 1;
                    if policy.is_recursive() && entry.attrs().is_dir() {
                        if depth >= walk.depth_limit() {
                            if !walk.truncates_at_depth_limit() {
                                page_error = Some(
                                    vfsi_core::api::Error::client(index, libc::EFBIG as u32)
                                        .with_context("visit_dirs", entry.path()),
                                );
                                break;
                            }
                        } else {
                            children.push((
                                index,
                                entry.path().to_path_buf(),
                                depth + 1,
                                seeds.remove(entry.path()),
                            ));
                        }
                    }
                }
                if let Some(error) = &page_error {
                    if accepted == 0 {
                        return Err(error.clone());
                    }
                    page.entries.truncate(accepted);
                }
                if !policy.is_recursive() && completed.len() <= index {
                    completed.resize(index + 1, crate::TraversalCompletion::Stopped);
                }
                if callback(index, page)
                    .map_err(|error| vector_index(error, index))?
                    .is_break()
                {
                    if policy.is_recursive() {
                        completed.truncate(index);
                        completed.push(crate::TraversalCompletion::Stopped);
                    } else {
                        completed[index] = crate::TraversalCompletion::Stopped;
                    }
                    return Ok(completed);
                }
                if let Some(error) = page_error {
                    return Err(error);
                }
                match next {
                    Some(cursor) => continuations.push(((index, path, depth, false), Some(cursor))),
                    None if !policy.is_recursive() => {
                        completed[index] = crate::TraversalCompletion::Complete
                    }
                    None => {}
                }
            }
            if continuations.is_empty() {
                break;
            }
            let cursors = continuations
                .iter_mut()
                .map(|state| state.1.take())
                .collect();
            let paths: Vec<_> = continuations
                .iter()
                .map(|state| state.0.1.as_path())
                .collect();
            let requested = budget.entries.saturating_add(1);
            let pages = fetch(&paths, cursors, requested.min(128), requested).map_err(|error| {
                error.map_index(|i| continuations.get(i).map_or(i, |state| state.0.0))
            })?;
            if pages.len() != continuations.len() {
                return Err(vfsi_core::api::Error::transport(
                    None,
                    "invalid directory continuation count",
                ));
            }
            wave = continuations
                .into_iter()
                .map(|state| state.0)
                .zip(pages)
                .collect();
        }
        if let Some(error) = deferred_error {
            return Err(error);
        }
        // Do not hold fallback snapshots while descending; all pages above
        // were consumed and their cursors released before adding the frontier.
        for child in children.into_iter().rev() {
            pending.push_front(child);
        }
        if policy.is_recursive() && pending.front().is_none_or(|state| state.0 != owner) {
            completed.push(crate::TraversalCompletion::Complete);
        }
    }
    Ok(completed)
}

impl<F: crate::backend::HandleBackend> FileHandle for crate::FsFile<F> {
    crate::__vfsi_file_methods!(crate::FsFile<F>);
}

impl<F: crate::backend::VectorBackend> vfsi_core::api::DirHandle for crate::FsDir<F> {
    crate::__vfsi_file_methods!(crate::FsDir<F>);
}

impl<F: crate::backend::VectorBackend + 'static> Vfsi for crate::FsClient<F> {
    type File = crate::FsFile<F>;
    type Dir = crate::FsDir<F>;
    crate::__vfsi_client_methods!(
        crate::FsClient<F>,
        std::convert::identity,
        read_backend::<F>,
        std::convert::identity,
        write_backend::<F>,
        write_backend_all::<F>,
        metadata_backend::<F, _>,
        std::convert::identity,
        |client: &Self, _: &[&Path]| client.directory_page_batch_size(),
        Self::open_with_native,
        Self::read_stream_with_options
    );
}

/// Shared owned-read dispatcher for native clients and their opaque facades.
#[doc(hidden)]
pub fn read_backend_owned<F: crate::backend::VectorBackend + 'static>(
    client: &crate::FsClient<F>,
    requests: &[ReadRequest<'_, crate::FsRead<'_, F>>],
    budget: usize,
) -> Result<Vec<OwnedReadResult>> {
    if requests.iter().all(|request| request.range_ref().is_some()) {
        return client.vread_with_limit_projected_native(requests, budget, |request| {
            request.range_ref().expect("checked ranges")
        });
    }
    read_batch(
        requests,
        budget,
        |ranges, bytes| client.vread_with_limit_projected_native(ranges, bytes, |request| request),
        |paths, bytes| {
            client.read_files_native(paths, crate::ReadAllOptions::new().max_total_bytes(bytes))
        },
    )
}
pub(crate) fn read_backend<'a, F: crate::backend::VectorBackend + 'static>(
    client: &crate::FsClient<F>,
    ops: impl IntoIterator<Item = ReadOp<'a, crate::FsFile<F>>>,
    options: ReadOptions,
) -> Result<Vec<ReadResult>> {
    consume_ops(
        ops,
        options.limit_or(client.limits().read_byte_limit()),
        crate::FsFile::read_request_at,
        crate::FsFile::read_request_at_into,
        |requests, options| read_backend_owned(client, requests, options),
        |requests, bytes| client.vread_into_with_limit_native(requests, bytes),
    )
}

// Keep low-level completion machinery private without leaking its historical
// operation name through application errors. Preserve status, index, and path.
#[doc(hidden)]
pub fn public_write_error(error: vfsi_core::api::Error) -> vfsi_core::api::Error {
    if error.operation() == Some("vwrite_all_native")
        && let Some(path) = error.path().map(std::path::Path::to_path_buf)
    {
        error.with_context("vwrite_native", path)
    } else {
        error
    }
}

pub(crate) fn write_backend<'a, F>(
    client: &crate::FsClient<F>,
    ops: &[WriteOp<'a, crate::FsFile<F>>],
) -> vfsi_core::api::Result<Vec<vfsi_core::api::WriteResult>>
where
    F: crate::backend::VectorBackend + 'static,
{
    client.vwrite_mapped_native(ops, |op| op.file().write_request_at(op.offset(), op.data()))
}

pub(crate) fn write_backend_all<'a, F>(
    client: &crate::FsClient<F>,
    ops: &[WriteOp<'a, crate::FsFile<F>>],
) -> vfsi_core::api::Result<Vec<vfsi_core::api::WriteResult>>
where
    F: crate::backend::VectorBackend + 'static,
{
    client.vwrite_all_mapped_native(ops, |op| op.file().write_request_at(op.offset(), op.data()))
}

pub(crate) fn metadata_backend<F, P: vfsi_core::AsTarget<crate::FsFile<F>>>(
    client: &crate::FsClient<F>,
    paths: &[P],
    options: AttrsOptions,
) -> vfsi_core::api::Result<Vec<crate::Attrs>>
where
    F: crate::backend::VectorBackend + 'static,
{
    client.vgetattrs_native(
        paths,
        options.requested_attributes(),
        options.follows_symlinks(),
    )
}

use vfsi_core::api::internal::{OwnedReadResult, ReadRequest, consume_ops, read_batch};
#[cfg(test)]
mod traversal_tests {
    use super::*;
    use crate::{AttrMask, DirEntry, DirPageCursor, VfAttrs, VfFile, VfType};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    // The fallback cursor owns the unconsumed listing. Track retained snapshots
    // while exercising the same visitor the Vfsi adapter uses in production.
    struct Snapshot {
        remaining: std::vec::IntoIter<DirEntry>,
        live: Arc<AtomicUsize>,
    }

    impl Drop for Snapshot {
        fn drop(&mut self) {
            self.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[allow(clippy::type_complexity)] // Mirrors the page-fetch callback contract under test.
    fn snapshot_pages(
        live: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    ) -> impl FnMut(
        &[&Path],
        Vec<Option<DirPageCursor>>,
        usize,
        usize,
    ) -> Result<Vec<crate::DirectoryPage>> {
        move |paths, cursors, page_size, max_entries| {
            let mut pages = Vec::with_capacity(paths.len());
            for (path, cursor) in paths.iter().zip(cursors) {
                let mut snapshot = match cursor {
                    Some(cursor) => cursor.into_state::<Snapshot>()?,
                    None => {
                        let depth = path.components().count();
                        let entries = (0..129)
                            .take(max_entries)
                            .map(|index| {
                                let path = path.join(format!("entry-{index}"));
                                let attrs = VfAttrs {
                                    file: VfFile::from_os_path(&path),
                                    ftype: if index == 0 && depth < 5 {
                                        VfType::Directory
                                    } else {
                                        VfType::Regular
                                    },
                                    masks: AttrMask::MODE,
                                    ..Default::default()
                                };
                                DirEntry::new(path, vfsi_core::metadata_from_attrs(attrs))
                            })
                            .collect::<Vec<_>>();
                        peak.fetch_max(live.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                        Snapshot {
                            remaining: entries.into_iter(),
                            live: Arc::clone(&live),
                        }
                    }
                };
                let entries = snapshot.remaining.by_ref().take(page_size).collect();
                let next = if snapshot.remaining.len() == 0 {
                    None
                } else {
                    Some(DirPageCursor::new(snapshot))
                };
                pages.push((
                    crate::DirectoryListing {
                        path: path.to_path_buf(),
                        entries,
                    },
                    next,
                    Vec::new(),
                ));
            }
            Ok(pages)
        }
    }

    fn visit(
        live: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        options: ListDirOptions,
        callback: impl FnMut(usize, crate::DirectoryListing) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<Vec<crate::TraversalCompletion>> {
        visit_directory_pages(
            &["/tree"],
            options,
            ResourceLimits::default(),
            |_| Ok(1),
            |_| Ok(()),
            snapshot_pages(live, peak),
            callback,
        )
    }

    #[test]
    fn recursive_visit_releases_fallback_snapshots_before_descending() {
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut seen = 0;
        let completions = visit(
            Arc::clone(&live),
            Arc::clone(&peak),
            ListDirOptions::new()
                .recursive(true)
                .max_entries(1000)
                .max_path_bytes(1_000_000),
            |_, page| {
                seen += page.entries.len();
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .unwrap();

        assert_eq!(completions, [crate::TraversalCompletion::Complete]);
        assert_eq!(seen, 4 * 129);
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn recursive_visit_drops_fallback_snapshot_on_stop_error_and_limit() {
        for outcome in 0..4 {
            let live = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let options = ListDirOptions::new()
                .recursive(true)
                .max_entries(if outcome == 2 { 2 } else { 1000 })
                // Root (5 bytes) and two entry paths (13 bytes each) fit.
                .max_path_bytes(if outcome == 3 { 31 } else { 1_000_000 });
            let mut fetch = snapshot_pages(Arc::clone(&live), peak);
            let mut callbacks = 0;
            let mut live_budget_page = false;
            let result = visit_directory_pages(
                &["/tree"],
                options,
                ResourceLimits::default(),
                |_| Ok(1),
                |_| Ok(()),
                |paths, cursors, page_size, max_entries| {
                    // A legal short page leaves a live snapshot when the byte
                    // limit rejects its second entry, after accepting the first.
                    let pages = fetch(paths, cursors, page_size.min(2), max_entries)?;
                    if outcome == 3 && pages[0].0.entries.len() == 2 {
                        assert!(pages[0].1.is_some());
                        live_budget_page = true;
                    }
                    Ok(pages)
                },
                |_, page| {
                    if outcome != 2 {
                        assert_eq!(live.load(Ordering::SeqCst), 1);
                    }
                    assert_eq!(page.entries.len(), 1);
                    assert_eq!(
                        page.entries[0].path(),
                        Path::new(&format!("/tree/entry-{callbacks}"))
                    );
                    callbacks += 1;
                    match outcome {
                        0 => Ok(std::ops::ControlFlow::Break(())),
                        1 => Err(vfsi_core::api::Error::client(0, libc::EIO as u32)),
                        _ => Ok(std::ops::ControlFlow::Continue(())),
                    }
                },
            );
            match outcome {
                0 => assert_eq!(result.unwrap(), [crate::TraversalCompletion::Stopped]),
                1 => assert_eq!(result.unwrap_err().err_no(), libc::EIO as u32),
                _ => {
                    let error = result.unwrap_err();
                    assert_eq!(error.err_no(), libc::EFBIG as u32);
                    assert_eq!(error.path(), Some(Path::new("/tree/entry-2")));
                    if outcome == 3 {
                        assert!(live_budget_page);
                    }
                }
            }
            assert_eq!(callbacks, if outcome >= 2 { 2 } else { 1 });
            assert_eq!(live.load(Ordering::SeqCst), 0);
        }
    }
}
