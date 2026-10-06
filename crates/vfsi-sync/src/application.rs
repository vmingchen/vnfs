//! Native execution of the portable VFSI contract.
use std::io::SeekFrom;
use std::path::Path;
use vfsi_core::api::*;
fn vector_index(error: vfsi_core::api::Error, index: usize) -> vfsi_core::api::Error {
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

macro_rules! file_methods {
    ($file:ty) => {
        fn path(&self) -> &Path {
            <$file>::path(self)
        }
        fn attrs(&self) -> Result<Attrs> {
            <$file>::attrs(self)
        }
        fn read_request_at(&self, offset: u64, length: usize) -> Self::ReadRequest<'_> {
            <$file>::read_request_at(self, offset, length)
        }
        fn read_request_at_into<'a>(
            &'a self,
            offset: u64,
            buffer: &'a mut [u8],
        ) -> Self::ReadIntoRequest<'a> {
            <$file>::read_request_at_into(self, offset, buffer)
        }
        fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<usize> {
            <$file>::read_at(self, buffer, offset)
        }
        fn write_at(&self, buffer: &[u8], offset: u64) -> Result<usize> {
            <$file>::write_at(self, buffer, offset)
        }
        fn read_native(&mut self, buffer: &mut [u8]) -> Result<usize> {
            <$file>::read_native(self, buffer)
        }
        fn read_to_end_with_limit(&mut self, max_bytes: usize) -> Result<Vec<u8>> {
            <$file>::read_to_end_with_limit(self, max_bytes)
        }
        fn write_native(&mut self, buffer: &[u8]) -> Result<usize> {
            <$file>::write_native(self, buffer)
        }
        fn seek_native(&mut self, position: SeekFrom) -> Result<u64> {
            <$file>::seek_native(self, position)
        }
        fn sync_data(&self) -> Result<()> {
            <$file>::sync_data(self)
        }
        fn sync_all(&self) -> Result<()> {
            <$file>::sync_all(self)
        }
        fn try_close(&mut self) -> Result<()> {
            <$file>::try_close(self)
        }
        fn is_closed(&self) -> bool {
            <$file>::is_closed(self)
        }
        fn close(self) -> Result<()> {
            <$file>::close(self)
        }
    };
}

impl<F: crate::FileSystem> FileHandle for crate::FsFile<F> {
    type ReadRequest<'a>
        = crate::FsRead<'a, F>
    where
        Self: 'a;
    type ReadIntoRequest<'a>
        = crate::FsReadInto<'a, F>
    where
        Self: 'a;
    file_methods!(crate::FsFile<F>);
}

pub(crate) trait NativeHooks: Vfsi {
    fn page_capacity(&self, paths: &[&Path]) -> Result<usize>;
    fn open_native(&self, request: OpenOp) -> Result<Self::File>;
    fn stream_native(
        &self,
        path: impl AsRef<Path>,
        options: crate::StreamOptions,
        callback: impl FnMut(u64, &[u8]) -> Result<bool>,
    ) -> Result<crate::StreamCompletion>;
}

macro_rules! client_methods {
    ($client:ty, $receiver:path) => {
        client_methods!($client, $receiver, <$client>::vread);
    };
    ($client:ty, $receiver:path, $vread_native:expr) => {
        client_methods!($client, $receiver, $vread_native, $receiver);
    };
    ($client:ty, $receiver:path, $vread_native:expr, $read_receiver:path) => {
        client_methods!(
            $client,
            $receiver,
            $vread_native,
            $read_receiver,
            <$client>::write_partial_native,
            <$client>::write_complete
        );
    };
    ($client:ty, $receiver:path, $vread_native:expr, $read_receiver:path, $vwrite_native:expr, $vwrite_all_native:expr) => {
        client_methods!(
            $client,
            $receiver,
            $vread_native,
            $read_receiver,
            $vwrite_native,
            $vwrite_all_native,
            <$client>::vgetattrs
        );
    };
    ($client:ty, $receiver:path, $vread_native:expr, $read_receiver:path, $vwrite_native:expr, $vwrite_all_native:expr, $metadata:expr) => {
        client_methods!(
            $client,
            $receiver,
            $vread_native,
            $read_receiver,
            $vwrite_native,
            $vwrite_all_native,
            $metadata,
            $receiver
        );
    };
    ($client:ty, $receiver:path, $vread_native:expr, $read_receiver:path, $vwrite_native:expr, $vwrite_all_native:expr, $metadata:expr, $write_receiver:path) => {
        client_methods!(
            $client,
            $receiver,
            $vread_native,
            $read_receiver,
            $vwrite_native,
            $vwrite_all_native,
            $metadata,
            $write_receiver,
            vrename,
            vcopy,
            vclose,
            vopen
        );
    };
    // Application clients and backend clients use different native method names.
    ($client:ty, $receiver:path, $vread_native:expr, $read_receiver:path, $vwrite_native:expr, $vwrite_all_native:expr, $metadata:expr, $write_receiver:path, $rename:ident, $copy:ident, $close:ident, $open_batch:ident) => {
        fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
            &self,
            pairs: &[(P, Q)],
            options: vfsi_core::api::RenameOptions,
        ) -> Result<()> {
            <$client>::vrename($receiver(self), pairs, options)
        }
        fn vlistdirs<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: vfsi_core::api::ListDirOptions,
            callback: impl FnMut(usize, DirectoryListing) -> Result<std::ops::ControlFlow<()>>,
        ) -> Result<Vec<crate::TraversalCompletion>> {
            visit_directory_pages(
                paths,
                options,
                self.limits(),
                |paths| <$client as NativeHooks>::page_capacity($receiver(self), paths),
                |path| {
                    let metadata = self.vgetattrs(
                        &[path],
                        vfsi_core::api::AttrsOptions::new()
                            .fields(vfsi_core::api::Attributes::MODE)
                            .follow_symlinks(false),
                    )?;
                    if metadata.len() != 1 {
                        return Err(vfsi_core::api::Error::transport(
                            None,
                            "invalid directory root metadata count",
                        ));
                    }
                    if !metadata[0].is_dir() {
                        return Err(vfsi_core::api::Error::client(0, libc::ENOTDIR as u32)
                            .with_context("visit_dirs", path));
                    }
                    Ok(())
                },
                |paths, cursors, page_size, max_entries| {
                    <$client>::read_dir_pages_with_fields(
                        $receiver(self),
                        paths,
                        options.attributes(),
                        cursors,
                        page_size,
                        max_entries,
                    )
                },
                callback,
            )
        }
        fn vstream<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: crate::StreamOptions,
            mut callback: impl FnMut(usize, u64, &[u8]) -> Result<bool>,
        ) -> Result<Vec<crate::StreamCompletion>> {
            let mut output = Vec::new();
            for (index, path) in paths.iter().enumerate() {
                let completion = <$client as NativeHooks>::stream_native(
                    $receiver(self),
                    path,
                    options,
                    |offset, data| callback(index, offset, data),
                )
                .map_err(|error| vector_index(error, index))?;
                output.push(completion);
                if matches!(completion, crate::StreamCompletion::Stopped { .. }) {
                    break;
                }
            }
            Ok(output)
        }
        fn capabilities(&self) -> Result<vfsi_core::Capabilities> {
            <$client>::capabilities($receiver(self))
        }
        fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()> {
            <$client>::vsymlink($receiver(self), pairs)
        }
        fn vreadlink<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<std::path::PathBuf>> {
            <$client>::vreadlink($receiver(self), paths)
        }
        fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()> {
            <$client>::vhardlink($receiver(self), pairs)
        }
        fn vstatfs<P: vfsi_core::MetadataOperand<Self::File>>(
            &self,
            targets: &[P],
        ) -> Result<Vec<vfsi_core::FilesystemStats>> {
            <$client>::vstatfs($receiver(self), targets)
        }
        fn vsetattrs<P: vfsi_core::MetadataOperand<Self::File>>(
            &self,
            updates: &[(P, vfsi_core::MetadataUpdate)],
            follow_symlinks: bool,
        ) -> Result<()> {
            <$client>::vsetattrs($receiver(self), updates, follow_symlinks)
        }
        fn vgetattrs<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: vfsi_core::api::AttrsOptions,
        ) -> Result<Vec<Attrs>> {
            ($metadata)($receiver(self), paths, options)
        }
        fn limits(&self) -> ResourceLimits {
            <$client>::limits($receiver(self))
        }

        fn vopen(&self, requests: &[OpenOp]) -> Result<Vec<Self::File>> {
            if requests.len() == 1 {
                // Preserve native symlink resolution and independent-handle state.
                return <$client as NativeHooks>::open_native($receiver(self), requests[0].clone())
                    .map(|file| vec![file]);
            }
            <$client>::$open_batch($receiver(self), requests)
        }
        fn vread<'a>(
            &self,
            ops: impl IntoIterator<Item = vfsi_core::api::ReadOp<'a, Self::File>>,
            options: vfsi_core::api::ReadOptions,
        ) -> Result<Vec<vfsi_core::api::ReadResult>> {
            ($vread_native)($read_receiver(self), ops, options)
        }
        fn vwrite<'a>(
            &self,
            requests: &[vfsi_core::api::WriteOp<'a, Self::File>],
            options: vfsi_core::api::WriteOptions,
        ) -> Result<Vec<WriteResult>> {
            let result = if options.writes_all() {
                ($vwrite_all_native)($write_receiver(self), requests)
            } else {
                ($vwrite_native)($write_receiver(self), requests)
            };
            result.map_err(public_write_error)
        }
        fn vclose(&self, files: &mut [Self::File]) -> Result<()> {
            <$client>::$close($receiver(self), files)
        }
        fn vmkdir<P: AsRef<Path>>(&self, paths: &[(P, u32)]) -> Result<()> {
            <$client>::vmkdir($receiver(self), paths)
        }

        fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(
            &self,
            pairs: &[(P, Q)],
            options: vfsi_core::api::CopyOption,
        ) -> Result<()> {
            <$client>::$copy($receiver(self), pairs, options)
        }
        fn vremove<P: AsRef<Path>>(
            &self,
            paths: &[P],
            mode: vfsi_core::api::RemoveMode,
            options: crate::RemoveOptions,
        ) -> Result<()> {
            match mode {
                vfsi_core::api::RemoveMode::Entry | vfsi_core::api::RemoveMode::Tree => {
                    <$client>::vremove_with_options_native(
                        $receiver(self),
                        paths,
                        mode == vfsi_core::api::RemoveMode::Tree,
                        options,
                    )
                }
                vfsi_core::api::RemoveMode::Contents => {
                    let mut first_error = None;
                    for (index, path) in paths.iter().enumerate() {
                        if let Err(error) = <$client>::remove_dir_contents_with_options(
                            $receiver(self),
                            path,
                            options,
                        ) {
                            let error = vector_index(error, index);
                            if error.is_transport() || !options.continues_on_error() {
                                return Err(error);
                            }
                            first_error.get_or_insert(error);
                        }
                    }
                    first_error.map_or(Ok(()), Err)
                }
            }
        }
    };
}

impl<F: crate::Backend + 'static> Vfsi for crate::FsClient<F> {
    type File = crate::FsFile<F>;
    client_methods!(
        crate::FsClient<F>,
        std::convert::identity,
        read_backend::<F>,
        std::convert::identity,
        write_backend::<F>,
        write_backend_all::<F>,
        metadata_backend::<F, _>,
        std::convert::identity,
        vrename,
        vcopy,
        vclose,
        vopen
    );
}

impl<F: crate::Backend + 'static> NativeHooks for crate::FsClient<F> {
    fn open_native(&self, request: OpenOp) -> Result<Self::File> {
        self.open_with_native(request)
    }
    fn page_capacity(&self, _paths: &[&Path]) -> Result<usize> {
        self.directory_page_batch_size()
    }
    fn stream_native(
        &self,
        path: impl AsRef<Path>,
        options: crate::StreamOptions,
        callback: impl FnMut(u64, &[u8]) -> Result<bool>,
    ) -> Result<crate::StreamCompletion> {
        self.read_stream_with_options(path, options, callback)
    }
}

pub(crate) fn read_backend_owned<F: crate::Backend + 'static>(
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
pub(crate) fn read_backend<'a, F: crate::Backend + 'static>(
    client: &crate::FsClient<F>,
    ops: impl IntoIterator<Item = ReadOp<'a, crate::FsFile<F>>>,
    options: ReadOptions,
) -> Result<Vec<ReadResult>> {
    consume_ops(
        ops,
        options.limit_or(client.limits().max_read_bytes),
        |requests, options| read_backend_owned(client, requests, options),
        |requests, bytes| client.vread_into_with_limit_native(requests, bytes),
    )
}

// Keep low-level completion machinery private without leaking its historical
// operation name through application errors. Preserve status, index, and path.
pub(crate) fn public_write_error(error: vfsi_core::api::Error) -> vfsi_core::api::Error {
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
    F: crate::Backend + 'static,
{
    client.vwrite_mapped_native(ops, |op| op.file().write_request_at(op.offset(), op.data()))
}

pub(crate) fn write_backend_all<'a, F>(
    client: &crate::FsClient<F>,
    ops: &[WriteOp<'a, crate::FsFile<F>>],
) -> vfsi_core::api::Result<Vec<vfsi_core::api::WriteResult>>
where
    F: crate::Backend + 'static,
{
    client.vwrite_all_mapped_native(ops, |op| op.file().write_request_at(op.offset(), op.data()))
}

pub(crate) fn metadata_backend<F, P: AsRef<std::path::Path>>(
    client: &crate::FsClient<F>,
    paths: &[P],
    options: AttrsOptions,
) -> vfsi_core::api::Result<Vec<crate::Attrs>>
where
    F: crate::Backend + 'static,
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
        for outcome in 0..3 {
            let live = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let options = ListDirOptions::new()
                .recursive(true)
                .max_entries(if outcome == 2 { 2 } else { 1000 })
                .max_path_bytes(1_000_000);
            let result = visit(Arc::clone(&live), peak, options, |_, _| match outcome {
                0 => Ok(std::ops::ControlFlow::Break(())),
                1 => Err(vfsi_core::api::Error::client(0, libc::EIO as u32)),
                _ => Ok(std::ops::ControlFlow::Continue(())),
            });
            match outcome {
                0 => assert_eq!(result.unwrap(), [crate::TraversalCompletion::Stopped]),
                1 => assert_eq!(result.unwrap_err().err_no(), libc::EIO as u32),
                _ => assert_eq!(result.unwrap_err().err_no(), libc::EFBIG as u32),
            }
            assert_eq!(live.load(Ordering::SeqCst), 0);
        }
    }
}
