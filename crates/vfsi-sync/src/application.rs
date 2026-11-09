//! Native execution of the portable VFSI contract.
use std::path::Path;
use vfsi_core::api::*;
use vfsi_core::internal::TraversalBudget;

/// Translate one READ outcome without copying an owned error prefix.
fn stream_read(
    result: vfsi_core::OpResult<ReadResult>,
    offset: u64,
    chunk: usize,
) -> (Option<ReadResult>, Option<vfsi_core::OpError>) {
    use vfsi_core::{FailureState, IoProgress, OpError};
    let invalid = || {
        OpError::unknown(
            Error::invalid_reply(None, "invalid stream read data, offset or progress"),
            None,
        )
    };
    match result {
        Ok(read) => {
            if read.data().is_none()
                || read.offset() != offset
                || read.bytes_read() > chunk
                || (read.bytes_read() == 0 && !read.eof())
            {
                (None, Some(invalid()))
            } else {
                (Some(read), None)
            }
        }
        Err(error) => {
            let (cause, state, progress) = error.into_parts();
            let prefix = match progress {
                None => None,
                Some(IoProgress::Read(read))
                    if state != FailureState::NotAttempted
                        && read.data().is_some()
                        && read.offset() == offset
                        && read.bytes_read() <= chunk =>
                {
                    Some(read)
                }
                Some(_) => return (None, Some(invalid())),
            };
            let error = match state {
                FailureState::Unknown => OpError::unknown(cause, None),
                // OPEN already succeeded for this stream.
                _ => OpError::failed(cause, None),
            };
            (prefix, Some(error))
        }
    }
}

/// Preserve failure knowledge and only the prefix accepted by callbacks.
fn stream_error(
    result: &mut vfsi_core::OpResult<StreamCompletion>,
    error: vfsi_core::OpError,
    offset: u64,
) {
    use vfsi_core::{FailureState, IoProgress, OpError};
    if result.is_err() {
        return;
    }
    let (cause, state, _) = error.into_parts();
    let progress = (offset != 0).then_some(IoProgress::Stream {
        next_offset: offset,
    });
    *result = Err(match state {
        FailureState::Unknown => OpError::unknown(cause, progress),
        _ => OpError::failed(cause, progress),
    });
}

/// Internal bounded-cohort implementation shared by native and opaque clients.
/// Keeps callback order unchanged; only the first chunk of siblings is prefetched.
#[doc(hidden)]
pub fn stream_cohorts<F: Vfsi, P: AsRef<Path>>(
    fs: &F,
    paths: &[P],
    options: StreamOptions,
    mut callback: impl FnMut(usize, u64, &[u8]) -> Result<std::ops::ControlFlow<()>>,
) -> vfsi_core::VecResult<StreamCompletion> {
    use vfsi_core::{IoProgress, OpError, internal::vec_result_slots};
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let memory = fs.limits().max_read_bytes();
    if memory == 0 {
        return Err(Error::errno(None, libc::EFBIG as u32));
    }
    let chunk = options.chunk_size().min(memory);
    let capacity = (memory / chunk).clamp(1, 32);
    let mut output: Vec<_> = (0..paths.len())
        .map(|_| Ok(StreamCompletion::Stopped { next_offset: 0 }))
        .collect();
    // Project user paths exactly once, outside any backend lock.
    let paths: Vec<_> = paths.iter().map(AsRef::as_ref).collect();
    let mut stopped = false;
    for (cohort, paths) in paths.chunks(capacity).enumerate() {
        let base = cohort * capacity;
        let requests: Vec<_> = paths
            .iter()
            .map(|path| OpenOp::new(*path, OpenFlags::READ))
            .collect();
        let opened = vec_result_slots(fs.vopen(&requests), paths.len());
        let mut files = Vec::with_capacity(opened.len());
        let mut indices = Vec::with_capacity(opened.len());
        for (local, outcome) in opened.into_iter().enumerate() {
            match outcome {
                Ok(file) => {
                    files.push(file);
                    indices.push(base + local);
                }
                Err(error) => {
                    output[base + local] = Err(if error.progress().is_some() {
                        OpError::unknown(
                            Error::invalid_reply(None, "stream OPEN contains byte progress"),
                            None,
                        )
                    } else {
                        error
                    });
                }
            }
        }
        let reads = if files.is_empty() {
            Vec::new()
        } else {
            vec_result_slots(
                fs.vread(
                    files.iter().map(|file| ReadOp::range(file, 0, chunk)),
                    ReadOptions::new(),
                ),
                files.len(),
            )
        };
        // Decode and retain all confirmed failures before callback cancellation.
        // Owned prefixes move into these slots, rather than being cloned.
        let first: Vec<_> = reads
            .into_iter()
            .enumerate()
            .map(|(local, read)| {
                let (read, failure) = stream_read(read, 0, chunk);
                if let Some(error) = failure {
                    stream_error(&mut output[indices[local]], error, 0);
                }
                read
            })
            .collect();
        let mut offsets = vec![0u64; files.len()];
        for (local, first) in first.into_iter().enumerate() {
            let index = indices[local];
            let mut read = first;
            while let Some(value) = read {
                let offset = offsets[local];
                let data = value.data().expect("validated owned stream chunk");
                let Some(next) = offset.checked_add(data.len() as u64) else {
                    stream_error(
                        &mut output[index],
                        OpError::failed(Error::errno(None, libc::EOVERFLOW as u32), None),
                        offset,
                    );
                    break;
                };
                if !data.is_empty() {
                    match callback(index, offset, data) {
                        Err(cause) => {
                            stream_error(&mut output[index], OpError::failed(cause, None), offset);
                            break;
                        }
                        Ok(decision) => {
                            offsets[local] = next;
                            // A read failure may accompany its confirmed prefix.
                            let previous = std::mem::replace(
                                &mut output[index],
                                Ok(StreamCompletion::Stopped { next_offset: next }),
                            );
                            if let Err(error) = previous {
                                let (cause, state, _) = error.into_parts();
                                output[index] = Err(if state == vfsi_core::FailureState::Unknown {
                                    OpError::unknown(
                                        cause,
                                        Some(IoProgress::Stream { next_offset: next }),
                                    )
                                } else {
                                    OpError::failed(
                                        cause,
                                        Some(IoProgress::Stream { next_offset: next }),
                                    )
                                });
                            }
                            if decision.is_break() {
                                stopped = true;
                                break;
                            }
                        }
                    }
                }
                if output[index].is_err() {
                    break;
                }
                if value.eof() {
                    output[index] = Ok(StreamCompletion::Complete);
                    break;
                }
                let offset = offsets[local];
                drop(value); // release before allocating the continuation
                let mut values = vec_result_slots(
                    fs.vread(
                        [ReadOp::range(&files[local], offset, chunk)],
                        ReadOptions::new(),
                    ),
                    1,
                );
                let (next, failure) = stream_read(values.pop().unwrap(), offset, chunk);
                if let Some(error) = failure {
                    stream_error(&mut output[index], error, offset);
                }
                read = next;
            }
            if stopped {
                break;
            }
        }
        // Close every acquired file, including unvisited speculative siblings.
        // Failed/ambiguous closes retain each handle's RAII cleanup ownership.
        if !files.is_empty() {
            let closes = vec_result_slots(fs.vclose(&mut files), files.len());
            for ((index, offset), result) in indices.into_iter().zip(offsets).zip(closes) {
                if let Err(error) = result {
                    let error = if error.progress().is_some() {
                        OpError::unknown(
                            Error::invalid_reply(None, "stream CLOSE contains byte progress"),
                            None,
                        )
                    } else {
                        error
                    };
                    stream_error(&mut output[index], error, offset);
                }
            }
        }
        if stopped {
            break;
        }
    }
    Ok(output)
}

fn directory_error(
    result: &mut vfsi_core::OpResult<crate::TraversalCompletion>,
    started: bool,
    mut error: vfsi_core::OpError,
) {
    use vfsi_core::{FailureState, OpError};
    if error.progress().is_some() {
        error = OpError::unknown(
            Error::invalid_reply(None, "directory traversal cannot report byte progress"),
            None,
        );
    } else if started && error.state() == FailureState::NotAttempted {
        error = OpError::failed(error.into_cause(), None);
    }
    if result.as_ref().err().is_none_or(|previous| {
        previous.state() != FailureState::Unknown && error.state() == FailureState::Unknown
    }) {
        *result = Err(error);
    }
}

pub fn visit_directory_pages<P: AsRef<Path>, C: Into<ListDirControl>>(
    roots: &[P],
    policy: vfsi_core::api::ListDirOptions,
    limits: ResourceLimits,
    mut capacity: impl FnMut(&[&Path]) -> Result<usize>,
    mut validate_root: impl FnMut(&Path) -> vfsi_core::OpResult<()>,
    mut fetch: impl FnMut(
        &[&Path],
        Vec<Option<crate::DirPageCursor>>,
        usize,
        usize,
        bool,
    ) -> vfsi_core::VecResult<crate::DirectoryPage>,
    mut callback: impl FnMut(usize, DirectoryListing) -> Result<C>,
) -> vfsi_core::VecResult<crate::TraversalCompletion> {
    use vfsi_core::{FailureState, OpError, internal::vec_result_slots};
    let directory = policy.directory_options(limits);
    let walk = policy.walk_options(limits);
    let mut budget = TraversalBudget::new(
        directory.max_entries_or(vfsi_core::api::DEFAULT_DIRECTORY_MAX_ENTRIES),
        directory.max_path_bytes_or(vfsi_core::api::DEFAULT_DIRECTORY_MAX_PATH_BYTES),
    );
    // Stopped is also the intentional-cancellation result for unvisited roots.
    // Started tracks current-call work, not a completed-prefix count.
    let mut completed: Vec<_> = (0..roots.len())
        .map(|_| Ok(crate::TraversalCompletion::Stopped))
        .collect();
    let mut started = vec![false; roots.len()];
    let mut pending = std::collections::VecDeque::new();
    for (index, root) in roots.iter().enumerate() {
        pending.push_back((index, root.as_ref().to_path_buf(), 0usize, None));
    }
    while !pending.is_empty() {
        while pending
            .front()
            .is_some_and(|state| completed[state.0].is_err())
        {
            pending.pop_front();
        }
        let Some(front) = pending.front() else {
            break;
        };
        let owner = front.0;
        if policy.recursive() && front.2 == 0 {
            if let Err(error) = validate_root(&front.1) {
                let attempted = error.state() != FailureState::NotAttempted;
                directory_error(&mut completed[owner], started[owner], error);
                started[owner] |= attempted;
                continue;
            }
            started[owner] = true;
        }
        // Recursive traversal retains its existing per-root frontier and
        // sibling batches. An ordinary root failure does not cancel other roots.
        let candidates: Vec<_> = pending
            .iter()
            .take_while(|state| !policy.recursive() || state.0 == owner)
            .take(32)
            .map(|state| state.1.as_path())
            .collect();
        let width = match capacity(&candidates) {
            Ok(width) => width.clamp(1, 32),
            Err(cause) => {
                directory_error(
                    &mut completed[owner],
                    started[owner],
                    OpError::not_attempted(cause),
                );
                continue;
            }
        };
        let size = if policy.recursive() {
            pending
                .iter()
                .take(width)
                .take_while(|state| state.0 == owner)
                .count()
        } else {
            pending.len().min(width)
        };
        let mut cohort: Vec<_> = pending.drain(..size).collect();
        let cursors = cohort.iter_mut().map(|state| state.3.take()).collect();
        let paths: Vec<_> = cohort.iter().map(|state| state.1.as_path()).collect();
        let requested = budget.remaining_entries().saturating_add(1);
        let follow = policy.allow_symlink_components() && cohort[0].2 == 0;
        let pages = vec_result_slots(fetch(&paths, cursors, 1, requested, follow), cohort.len());
        let mut wave: Vec<_> = cohort
            .into_iter()
            .map(|(index, path, depth, _)| (index, path, depth, true))
            .zip(pages)
            .collect();
        let mut children = Vec::new();
        while !wave.is_empty() {
            // Preserve every known failure from the submitted wave, even if a
            // successful earlier callback then asks to stop delivery.
            for ((index, _, _, _), result) in &mut wave {
                match result {
                    Ok(_) => started[*index] = true,
                    Err(error) => {
                        let error = std::mem::replace(
                            error,
                            OpError::not_attempted(Error::unsupported(None)),
                        );
                        let attempted = error.state() != FailureState::NotAttempted;
                        directory_error(&mut completed[*index], started[*index], error);
                        started[*index] |= attempted;
                    }
                }
            }
            let mut continuations = Vec::new();
            for ((index, path, depth, first), result) in wave {
                if completed[index].is_err() {
                    continue;
                }
                let (mut page, next, seeds) = result.expect("failed page roots were filtered");
                let malformed = |message| {
                    OpError::unknown(Error::invalid_reply(None, message), None)
                        .with_context("vlistdirs", &path)
                };
                if page.path != path
                    || (page.entries.is_empty() && next.is_some())
                    || page
                        .entries
                        .iter()
                        .any(|entry| entry.path().parent() != Some(path.as_path()))
                {
                    directory_error(
                        &mut completed[index],
                        started[index],
                        malformed("invalid directory page parent or progress"),
                    );
                    continue;
                }
                let mut seeds_by_path = std::collections::HashMap::new();
                let mut valid_seeds = true;
                for (seed_path, cursor) in seeds {
                    if seeds_by_path.insert(seed_path, (cursor, false)).is_some() {
                        valid_seeds = false;
                        break;
                    }
                }
                for entry in page.entries.iter().filter(|entry| entry.attrs().is_dir()) {
                    if let Some((_, observed)) = seeds_by_path.get_mut(entry.path()) {
                        *observed = true;
                    }
                }
                valid_seeds &= seeds_by_path.values().all(|(_, observed)| *observed);
                if !valid_seeds {
                    directory_error(
                        &mut completed[index],
                        started[index],
                        malformed("invalid child directory seed"),
                    );
                    continue;
                }
                if first
                    && policy.recursive()
                    && let Err(cause) = budget.charge_path(&path)
                {
                    directory_error(
                        &mut completed[index],
                        true,
                        OpError::failed(cause.with_context("vector traversal", &path), None),
                    );
                    continue;
                }
                let mut page_error = None;
                let mut candidates = Vec::new();
                let mut accepted = 0;
                for entry in &page.entries {
                    if let Err(cause) = budget.charge(entry.path()) {
                        page_error = Some(cause.with_context("vector traversal", entry.path()));
                        break;
                    }
                    accepted += 1;
                    if policy.recursive() && entry.attrs().is_dir() {
                        candidates.push((
                            index,
                            entry.path().to_path_buf(),
                            depth + 1,
                            seeds_by_path.remove(entry.path()).map(|(cursor, _)| cursor),
                        ));
                    }
                }
                if let Some(cause) = &page_error {
                    if accepted == 0 {
                        directory_error(
                            &mut completed[index],
                            true,
                            OpError::failed(cause.clone(), None),
                        );
                        continue;
                    }
                    page.entries.truncate(accepted);
                }
                let decision = match callback(index, page) {
                    Ok(decision) => decision.into(),
                    Err(cause) => {
                        directory_error(&mut completed[index], true, OpError::failed(cause, None));
                        continue;
                    }
                };
                if decision == ListDirControl::Stop {
                    return Ok(completed);
                }
                if let Some(cause) = page_error {
                    directory_error(&mut completed[index], true, OpError::failed(cause, None));
                    continue;
                }
                if let ListDirControl::Prune(paths) = decision {
                    if let Some(path) = paths
                        .iter()
                        .find(|path| !candidates.iter().any(|candidate| &candidate.1 == *path))
                    {
                        directory_error(
                            &mut completed[index],
                            true,
                            OpError::failed(
                                Error::from_kind(None, std::io::ErrorKind::InvalidInput)
                                    .with_context("vlistdirs prune", path),
                                None,
                            ),
                        );
                        continue;
                    }
                    candidates.retain(|candidate| !paths.contains(&candidate.1));
                }
                if depth >= walk.max_depth_or(Default::default()).get() && !candidates.is_empty() {
                    if !walk.truncate_at_max_depth() {
                        directory_error(
                            &mut completed[index],
                            true,
                            OpError::failed(
                                Error::errno(None, libc::EFBIG as u32)
                                    .with_context("visit_dirs", &candidates[0].1),
                                None,
                            ),
                        );
                        continue;
                    }
                } else {
                    children.extend(candidates);
                }
                match next {
                    Some(cursor) => continuations.push(((index, path, depth, false), Some(cursor))),
                    None if !policy.recursive() => {
                        completed[index] = Ok(crate::TraversalCompletion::Complete)
                    }
                    None => {}
                }
            }
            continuations.retain(|state| completed[state.0.0].is_ok());
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
            let requested = budget.remaining_entries().saturating_add(1);
            let follow = policy.allow_symlink_components() && continuations[0].0.2 == 0;
            let pages = vec_result_slots(
                fetch(&paths, cursors, requested.min(128), requested, follow),
                continuations.len(),
            );
            wave = continuations
                .into_iter()
                .map(|state| state.0)
                .zip(pages)
                .collect();
        }
        for child in children.into_iter().rev() {
            if completed[child.0].is_ok() {
                pending.push_front(child);
            }
        }
        if policy.recursive()
            && completed[owner].is_ok()
            && pending.front().is_none_or(|state| state.0 != owner)
        {
            completed[owner] = Ok(crate::TraversalCompletion::Complete);
        }
    }
    Ok(completed)
}

impl<F: crate::backend::HandleBackend> FileHandle for crate::OpenFile<F> {
    crate::__vfsi_file_methods!(crate::OpenFile<F>);
}

impl<F: crate::backend::VectorBackend> vfsi_core::api::DirHandle for crate::FsDir<F> {
    crate::__vfsi_file_methods!(crate::FsDir<F>);
}

impl<F: crate::backend::VectorBackend + 'static> Vfsi for crate::FsClient<F> {
    type File = crate::OpenFile<F>;
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
        |client: &Self, _: &[&Path]| client.directory_page_batch_size()
    );
}

/// Shared owned-read dispatcher for native clients and their opaque facades.
#[doc(hidden)]
pub fn read_backend_owned<F: crate::backend::VectorBackend + 'static>(
    client: &crate::FsClient<F>,
    requests: &[ReadRequest<'_, crate::FsRead<'_, F>>],
    budget: usize,
) -> vfsi_core::VecResult<OwnedReadResult> {
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
            client.read_files_native(
                paths,
                crate::ReadAllOptions::new().with_max_total_bytes(bytes),
            )
        },
    )
}
pub(crate) fn read_backend<'a, F: crate::backend::VectorBackend + 'static>(
    client: &crate::FsClient<F>,
    ops: impl IntoIterator<Item = ReadOp<'a, crate::OpenFile<F>>>,
    options: ReadOptions,
) -> vfsi_core::VecResult<ReadResult> {
    consume_ops(
        ops,
        options.max_total_bytes_or(client.limits().max_read_bytes()),
        crate::OpenFile::read_request_at,
        crate::OpenFile::read_request_at_into,
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

/// Normalize public WRITE diagnostics without losing completion or progress.
#[doc(hidden)]
pub fn public_write_results<T>(result: vfsi_core::VecResult<T>) -> vfsi_core::VecResult<T> {
    result.map_err(public_write_error).map(|outcomes| {
        outcomes
            .into_iter()
            .map(|outcome| {
                outcome.map_err(|error| {
                    if error.cause().operation() == Some("vwrite_all_native")
                        && let Some(path) = error.cause().path().map(std::path::Path::to_path_buf)
                    {
                        error.with_context("vwrite_native", path)
                    } else {
                        error
                    }
                })
            })
            .collect()
    })
}

pub(crate) fn write_backend<'a, F>(
    client: &crate::FsClient<F>,
    ops: &[WriteOp<'a, crate::OpenFile<F>>],
) -> vfsi_core::VecResult<vfsi_core::api::WriteResult>
where
    F: crate::backend::VectorBackend + 'static,
{
    client.vwrite_mapped_native(ops, |op| op.file().write_request_at(op.offset(), op.data()))
}

pub(crate) fn write_backend_all<'a, F>(
    client: &crate::FsClient<F>,
    ops: &[WriteOp<'a, crate::OpenFile<F>>],
) -> vfsi_core::VecResult<vfsi_core::api::WriteResult>
where
    F: crate::backend::VectorBackend + 'static,
{
    client.vwrite_all_mapped_native(ops, |op| op.file().write_request_at(op.offset(), op.data()))
}

pub(crate) fn metadata_backend<F, P: vfsi_core::AsTarget<crate::OpenFile<F>>>(
    client: &crate::FsClient<F>,
    paths: &[P],
    options: AttrsOptions,
) -> vfsi_core::VecResult<crate::Attrs>
where
    F: crate::backend::VectorBackend + 'static,
{
    client.vgetattrs_native(paths, options.fields(), options.follow_final_symlink())
}

use vfsi_core::api::internal::{OwnedReadResult, ReadRequest, consume_ops, read_batch};
#[cfg(test)]
mod traversal_tests {
    use super::*;
    use crate::{AttrMask, DirEntry, DirPageCursor, FileOperand, VfAttrs, VfType};
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
        bool,
    ) -> vfsi_core::VecResult<crate::DirectoryPage> {
        move |paths, cursors, page_size, max_entries, _follow_symlinks| {
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
                                    file: FileOperand::from_os_path(&path),
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
                pages.push(Ok((
                    crate::DirectoryListing {
                        path: path.to_path_buf(),
                        entries,
                    },
                    next,
                    Vec::new(),
                )));
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
        {
            let visit_roots = &["/tree"];
            vfsi_core::internal::collect_vec_result(
                visit_directory_pages(
                    visit_roots,
                    options,
                    ResourceLimits::default(),
                    |_| Ok(1),
                    |_| Ok(()),
                    snapshot_pages(live, peak),
                    callback,
                ),
                visit_roots.len(),
            )
        }
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
                .with_recursive(true)
                .with_max_entries(1000)
                .with_max_path_bytes(1_000_000),
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
                .with_recursive(true)
                .with_max_entries(if outcome == 2 { 2 } else { 1000 })
                // Root (5 bytes) and two entry paths (13 bytes each) fit.
                .with_max_path_bytes(if outcome == 3 { 31 } else { 1_000_000 });
            let mut fetch = snapshot_pages(Arc::clone(&live), peak);
            let mut callbacks = 0;
            let mut live_budget_page = false;
            let result = {
                let visit_roots = &["/tree"];
                vfsi_core::internal::collect_vec_result(
                    visit_directory_pages(
                        visit_roots,
                        options,
                        ResourceLimits::default(),
                        |_| Ok(1),
                        |_| Ok(()),
                        |paths, cursors, page_size, max_entries, follow_symlinks| {
                            // A legal short page leaves a live snapshot when the byte
                            // limit rejects its second entry, after accepting the first.
                            let pages = fetch(
                                paths,
                                cursors,
                                page_size.min(2),
                                max_entries,
                                follow_symlinks,
                            )?;
                            if outcome == 3 && pages[0].as_ref().unwrap().0.entries.len() == 2 {
                                assert!(pages[0].as_ref().unwrap().1.is_some());
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
                                1 => Err(vfsi_core::api::Error::errno(0, libc::EIO as u32)),
                                _ => Ok(std::ops::ControlFlow::Continue(())),
                            }
                        },
                    ),
                    visit_roots.len(),
                )
            };
            match outcome {
                0 => assert_eq!(result.unwrap(), [crate::TraversalCompletion::Stopped]),
                1 => assert_eq!(result.unwrap_err().raw_code(), libc::EIO as u32),
                _ => {
                    let error = result.unwrap_err();
                    assert_eq!(error.raw_code(), libc::EFBIG as u32);
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
    #[test]
    fn recursive_pages_keep_root_policy_and_reject_descendant_symlinks() {
        for recursive in [false, true] {
            for allow in [false, true] {
                let live = Arc::new(AtomicUsize::new(0));
                let peak = Arc::new(AtomicUsize::new(0));
                let mut fetch = snapshot_pages(live, peak);
                let mut observed = Vec::new();
                {
                    let visit_roots = &["/tree"];
                    vfsi_core::internal::collect_vec_result(
                        visit_directory_pages(
                            visit_roots,
                            ListDirOptions::new()
                                .with_recursive(recursive)
                                .with_allow_symlink_components(allow),
                            ResourceLimits::default(),
                            |_| Ok(1),
                            |_| Ok(()),
                            |paths, cursors, page_size, max_entries, follow| {
                                for path in paths {
                                    observed.push((path.to_path_buf(), follow));
                                }
                                fetch(paths, cursors, page_size, max_entries, follow)
                            },
                            |_, _| Ok(std::ops::ControlFlow::Continue(())),
                        ),
                        visit_roots.len(),
                    )
                }
                .unwrap();
                assert!(observed.len() > 1, "exercise continuation fetches");
                assert_eq!(
                    observed.iter().any(|(path, _)| path != Path::new("/tree")),
                    recursive
                );
                for (path, follow) in observed {
                    assert_eq!(follow, allow && path == Path::new("/tree"));
                }
            }
        }
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    struct Seed(PathBuf);
    struct RootPage;
    fn entry(path: &str) -> DirEntry {
        DirEntry::new(
            path.into(),
            vfsi_core::metadata_from_attrs(vfsi_core::VfAttrs {
                file: vfsi_core::FileOperand::from_path(path),
                masks: vfsi_core::AttrMask::MODE,
                ftype: vfsi_core::VfType::Directory,
                ..Default::default()
            }),
        )
    }
    fn walk(
        depth: usize,
        mut callback: impl FnMut(usize, DirectoryListing) -> Result<ListDirControl>,
    ) -> (Result<Vec<TraversalCompletion>>, Vec<Vec<PathBuf>>) {
        let waves = RefCell::new(Vec::new());
        let result = {
            let visit_roots = &["/tree"];
            vfsi_core::internal::collect_vec_result(
                visit_directory_pages(
                    visit_roots,
                    ListDirOptions::new()
                        .with_recursive(true)
                        .with_max_depth(DepthLimit::new(depth).unwrap()),
                    ResourceLimits::new(),
                    |_| Ok(2),
                    |_| Ok(()),
                    |paths, cursors, size, _, follow| {
                        waves
                            .borrow_mut()
                            .push(paths.iter().map(|p| p.to_path_buf()).collect());
                        Ok(paths
                            .iter()
                            .zip(cursors)
                            .map(|(path, cursor)| {
                                let (entries, next) = if *path == Path::new("/tree") {
                                    assert!(follow);
                                    if let Some(cursor) = cursor {
                                        cursor.into_state::<RootPage>()?;
                                        (vec![entry("/tree/prune"), entry("/tree/b")], None)
                                    } else {
                                        assert_eq!(size, 1);
                                        (
                                            vec![entry("/tree/a")],
                                            Some(crate::DirPageCursor::new(RootPage)),
                                        )
                                    }
                                } else {
                                    assert!(!follow);
                                    assert_ne!(
                                        *path,
                                        Path::new("/tree/prune"),
                                        "pruned before child I/O"
                                    );
                                    let seed = cursor
                                        .expect("must retain anchored child seed")
                                        .into_state::<Seed>()?;
                                    assert_eq!(seed.0, *path);
                                    (Vec::new(), None)
                                };
                                let seeds = entries
                                    .iter()
                                    .map(|entry| {
                                        (
                                            entry.path().to_path_buf(),
                                            crate::DirPageCursor::new(Seed(
                                                entry.path().to_path_buf(),
                                            )),
                                        )
                                    })
                                    .collect();
                                Ok((
                                    DirectoryListing {
                                        path: path.to_path_buf(),
                                        entries,
                                    },
                                    next,
                                    seeds,
                                ))
                            })
                            .map(|result: Result<_>| {
                                result.map_err(|cause| vfsi_core::OpError::failed(cause, None))
                            })
                            .collect())
                    },
                    &mut callback,
                ),
                visit_roots.len(),
            )
        };
        (result, waves.into_inner())
    }

    #[test]
    fn admission_keeps_anchored_sibling_batches_and_prunes_before_io() {
        let mut seen = Vec::new();
        let (result, waves) = walk(1, |index, page| {
            assert_eq!(index, 0);
            seen.push(page.path);
            Ok(ListDirControl::Prune(
                page.entries
                    .iter()
                    .filter(|entry| entry.file_name() == Some(std::ffi::OsStr::new("prune")))
                    .map(|entry| entry.path().to_path_buf())
                    .collect(),
            ))
        });
        assert_eq!(result.unwrap(), [TraversalCompletion::Complete]);
        assert_eq!(
            seen,
            ["/tree", "/tree", "/tree/a", "/tree/b"].map(PathBuf::from)
        );
        assert_eq!(
            waves,
            vec![
                vec![PathBuf::from("/tree")],
                vec![PathBuf::from("/tree")],
                vec![PathBuf::from("/tree/a"), PathBuf::from("/tree/b")]
            ]
        );
    }

    #[test]
    fn admission_stop_error_and_depth_pruning_never_fetch_children() {
        for mode in 0..5 {
            let mut calls = 0;
            let (result, waves) = walk(0, |_, page| {
                calls += 1;
                match mode {
                    0 => Ok(ListDirControl::Stop),
                    1 => Err(Error::errno(7, libc::ECANCELED as u32)),
                    2 => Ok(ListDirControl::Prune(
                        page.entries
                            .iter()
                            .map(|e| e.path().to_path_buf())
                            .collect(),
                    )),
                    3 => Ok(ListDirControl::Prune(vec!["/unrelated".into()])),
                    _ => Ok(ListDirControl::Continue),
                }
            });
            assert!(waves.iter().flatten().all(|p| p == Path::new("/tree")));
            match mode {
                0 => assert_eq!(result.unwrap(), [TraversalCompletion::Stopped]),
                1 => assert_eq!(result.unwrap_err().index(), Some(0), "input root index"),
                2 => {
                    assert_eq!(result.unwrap(), [TraversalCompletion::Complete]);
                    assert_eq!(calls, 2);
                }
                3 => assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput),
                _ => assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::FileTooLarge),
            }
        }
    }
}
