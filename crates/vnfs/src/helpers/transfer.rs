use crate::{
    Attributes, Attrs, AttrsOptions, ControlFlow, CopyOption, DepthLimit, Error, ListDirOptions,
    OpenFlags, OpenOp, ReadOp, ReadOptions, Result, SetAttrsOp, TransportKind, Vfsi, VfsiExt,
    WriteOp, WriteOptions,
};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// Destination-file policy. Existing directories are merged, never replaced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Existing {
    #[default]
    Error,
    Skip,
    Replace,
}
/// How directory operands are mapped by `copy_items`/`move_items`.
/// `copy_tree` always uses its explicit destination root instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CopyLayout {
    #[default]
    Container,
    Contents,
}
/// Symlinks and special objects are not copied or followed in this initial helper.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnsupportedEntry {
    #[default]
    Error,
    Skip,
}
#[bitfields::bitfield(u8)]
#[derive(PartialEq, Eq)]
struct CopyFlags {
    preserve_permissions: bool,
    #[bits(7)]
    _reserved: u8,
}
/// Bounded, nontransactional transfer policy. Defaults: fail on existing files,
/// reject links/special files, 64-file batches and 1 MiB chunks. Aggregate read
/// storage and traversal limits inherit the client's limits. Options never
/// disable negotiated backend limits. Failure/cancellation leaves partial output.
#[derive(Clone, Copy, Debug)]
pub struct CopyOptions {
    existing: Existing,
    layout: CopyLayout,
    unsupported: UnsupportedEntry,
    flags: CopyFlags,
    batch_size: usize,
    chunk_bytes: usize,
    max_entries: Option<usize>,
    max_path_bytes: Option<usize>,
    depth: Option<DepthLimit>,
}
impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            existing: Existing::Error,
            layout: CopyLayout::Container,
            unsupported: UnsupportedEntry::Error,
            flags: CopyFlags::new(),
            batch_size: 64,
            chunk_bytes: 1024 * 1024,
            max_entries: None,
            max_path_bytes: None,
            depth: None,
        }
    }
}
impl CopyOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn existing(mut self, value: Existing) -> Self {
        self.existing = value;
        self
    }
    pub fn layout(mut self, value: CopyLayout) -> Self {
        self.layout = value;
        self
    }
    pub fn unsupported_entries(mut self, value: UnsupportedEntry) -> Self {
        self.unsupported = value;
        self
    }
    /// Preserve file permission bits, not directory modes, ownership, times or ACLs.
    pub fn preserve_permissions(mut self, value: bool) -> Self {
        self.flags.set_preserve_permissions(value);
        self
    }
    pub fn batch_size(mut self, value: usize) -> Self {
        self.batch_size = value;
        self
    }
    pub fn chunk_bytes(mut self, value: usize) -> Self {
        self.chunk_bytes = value;
        self
    }
    pub fn max_entries(mut self, value: usize) -> Self {
        self.max_entries = Some(value);
        self
    }
    /// Aggregate source+destination logical path bytes over the whole transfer.
    pub fn max_path_bytes(mut self, value: usize) -> Self {
        self.max_path_bytes = Some(value);
        self
    }
    /// Deliberately omit deeper directories. Omitted subtrees count as skipped;
    /// move helpers then retain the source. Without this override, exceeding the
    /// client's depth limit is an error rather than successful truncation.
    pub fn depth(mut self, value: DepthLimit) -> Self {
        self.depth = Some(value);
        self
    }
}
/// No exact total-size prewalk is performed. Native COPY cannot report exact
/// bytes through Vfsi, so `bytes_copied` becomes None when that fast path is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferSummary {
    pub files_copied: u64,
    pub directories_processed: u64,
    pub entries_skipped: u64,
    pub bytes_copied: Option<u64>,
    pub roots_renamed: u64,
    pub roots_removed: u64,
    pub stopped: bool,
}
impl Default for TransferSummary {
    fn default() -> Self {
        Self {
            files_copied: 0,
            directories_processed: 0,
            entries_skipped: 0,
            bytes_copied: Some(0),
            roots_renamed: 0,
            roots_removed: 0,
            stopped: false,
        }
    }
}
/// Synchronous callback outside backend locks. Stop takes effect after the
/// current vector wave: writes already accepted for sibling files remain done.
#[derive(Clone, Copy, Debug)]
pub struct TransferProgress<'a> {
    pub source: &'a Path,
    pub destination: &'a Path,
    pub file_bytes_copied: u64,
    pub file_size: u64,
    pub summary: TransferSummary,
}
type Progress<'a> = Option<&'a mut dyn FnMut(TransferProgress<'_>) -> Result<ControlFlow<()>>>;

/// Copy mixed files/directories into a destination directory. Parents must exist.
/// Relative paths are interpreted from the client's root; `..` is rejected.
/// Rejects lexical overlap among sources and destinations. Ancestor aliases,
/// concurrent renames and symlink swaps are not sandboxed: use trusted, stable
/// namespaces. Replace unlinks existing regular destinations before exclusive
/// creation, avoiding writes through their hard links. Failure may leave missing
/// or partial output. No mode promises snapshotting, rollback or durability.
/// Descriptor copies read at most the size observed during discovery; concurrent
/// growth is ignored. Native COPY uses backend semantics instead.
pub fn copy_items<P: AsRef<Path>>(
    fs: &impl Vfsi,
    sources: &[P],
    destination: impl AsRef<Path>,
    options: CopyOptions,
) -> Result<TransferSummary> {
    run(fs, sources, destination.as_ref(), options, false, None)
}
pub fn copy_items_with_progress<P: AsRef<Path>>(
    fs: &impl Vfsi,
    sources: &[P],
    destination: impl AsRef<Path>,
    options: CopyOptions,
    mut progress: impl FnMut(TransferProgress<'_>) -> Result<ControlFlow<()>>,
) -> Result<TransferSummary> {
    run(
        fs,
        sources,
        destination.as_ref(),
        options,
        false,
        Some(&mut progress),
    )
}
/// Copy one source tree to an exact destination root (not destination/basename).
/// The source must be a directory. Other semantics match `copy_items`.
pub fn copy_tree(
    fs: &impl Vfsi,
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    options: CopyOptions,
) -> Result<TransferSummary> {
    run(fs, &[source], destination.as_ref(), options, true, None)
}
pub fn copy_tree_with_progress(
    fs: &impl Vfsi,
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    options: CopyOptions,
    mut progress: impl FnMut(TransferProgress<'_>) -> Result<ControlFlow<()>>,
) -> Result<TransferSummary> {
    run(
        fs,
        &[source],
        destination.as_ref(),
        options,
        true,
        Some(&mut progress),
    )
}

#[derive(Clone)]
struct Task {
    source: PathBuf,
    destination: PathBuf,
    metadata: Attrs,
    size: u64,
    root: usize,
    depth: usize,
    fresh_destination: bool,
}
struct Budget {
    entries: usize,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}
impl Budget {
    fn charge(&mut self, source: &Path, destination: &Path, root: usize) -> Result<()> {
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| limit(root, source))?;
        self.bytes = self
            .bytes
            .checked_add(source.as_os_str().len())
            .and_then(|n| n.checked_add(destination.as_os_str().len()))
            .ok_or_else(|| limit(root, source))?;
        if self.entries > self.max_entries || self.bytes > self.max_bytes {
            return Err(limit(root, source));
        }
        Ok(())
    }
}
fn invalid(root: usize, path: &Path) -> Error {
    Error::client(root, libc::EINVAL as u32).with_context("transfer", path)
}
fn limit(root: usize, path: &Path) -> Error {
    Error::client(root, libc::EFBIG as u32).with_context("transfer", path)
}
fn contract(message: &str) -> Error {
    Error::transport_with_kind(None, TransportKind::InvalidReply, message)
}
fn normalize(path: &Path, max: usize) -> Result<PathBuf> {
    if path.as_os_str().len() > max {
        return Err(Error::client(0, libc::EFBIG as u32));
    }
    if path.as_os_str().is_empty() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(invalid(0, path));
    }
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::RootDir | Component::CurDir => {}
            _ => return Err(invalid(0, path)),
        }
    }
    Ok(out)
}
fn fields() -> Attributes {
    Attributes::MODE | Attributes::SIZE | Attributes::FILEID
}
fn mapped(error: Error, tasks: &[impl std::borrow::Borrow<Task>]) -> Error {
    if let Some(task) = error
        .index()
        .and_then(|i| tasks.get(i))
        .map(std::borrow::Borrow::borrow)
    {
        error
            .with_index(task.root)
            .with_context("transfer", &task.source)
    } else {
        error
    }
}
fn roots<P: AsRef<Path>>(
    fs: &impl Vfsi,
    sources: &[P],
    destination: &Path,
    options: CopyOptions,
    exact: bool,
    budget: &mut Budget,
) -> Result<Vec<Task>> {
    if options.batch_size == 0 || options.chunk_bytes == 0 || fs.limits().read_byte_limit() == 0 {
        return Err(invalid(0, destination));
    }
    if sources.len() > budget.max_entries {
        return Err(limit(0, destination));
    }
    let destination = normalize(destination, budget.max_bytes)?;
    let mut tasks = Vec::with_capacity(sources.len());
    for (root, source) in sources.iter().enumerate() {
        let source =
            normalize(source.as_ref(), budget.max_bytes).map_err(|e| e.with_index(root))?;
        let dest = if exact || options.layout == CopyLayout::Contents {
            destination.clone()
        } else {
            destination.join(source.file_name().ok_or_else(|| invalid(root, &source))?)
        };
        if source.starts_with(&dest) || dest.starts_with(&source) {
            return Err(invalid(root, &source));
        }
        for previous in &tasks {
            let previous: &Task = previous;
            if source.starts_with(&previous.source)
                || previous.source.starts_with(&source)
                || dest.starts_with(&previous.source)
                || previous.source.starts_with(&dest)
                || source.starts_with(&previous.destination)
                || previous.destination.starts_with(&source)
                || (options.layout != CopyLayout::Contents
                    && (dest.starts_with(&previous.destination)
                        || previous.destination.starts_with(&dest)))
            {
                return Err(invalid(root, &source));
            }
        }
        budget.charge(&source, &dest, root)?;
        // Query one bounded root wave, without materializing a whole tree.
        tasks.push(Task {
            source,
            destination: dest,
            metadata: placeholder(),
            size: 0,
            root,
            depth: 0,
            fresh_destination: false,
        });
    }
    for batch in tasks.chunks_mut(options.batch_size) {
        let paths: Vec<_> = batch.iter().map(|t| &t.source).collect();
        let metadata = fs
            .vgetattrs(
                &paths,
                AttrsOptions::new().fields(fields()).follow_symlinks(false),
            )
            .map_err(|e| mapped(e, batch))?;
        if metadata.len() != batch.len() {
            return Err(contract("transfer: invalid metadata cardinality"));
        }
        for (task, metadata) in batch.iter_mut().zip(metadata) {
            if exact && !metadata.is_dir() {
                return Err(Error::client(task.root, libc::ENOTDIR as u32)
                    .with_context("transfer", &task.source));
            }
            if !metadata.is_dir()
                && !metadata.is_file()
                && options.unsupported == UnsupportedEntry::Error
            {
                return Err(Error::client(task.root, libc::ENOTSUP as u32)
                    .with_context("transfer", &task.source));
            }
            if options.layout == CopyLayout::Contents && metadata.is_file() && !exact {
                let mapped = destination.join(
                    task.source
                        .file_name()
                        .ok_or_else(|| invalid(task.root, &task.source))?,
                );
                budget.bytes = budget
                    .bytes
                    .checked_add(
                        mapped
                            .as_os_str()
                            .len()
                            .saturating_sub(task.destination.as_os_str().len()),
                    )
                    .ok_or_else(|| limit(task.root, &task.source))?;
                if budget.bytes > budget.max_bytes {
                    return Err(limit(task.root, &task.source));
                }
                task.destination = mapped;
            }
            task.metadata = metadata;
        }
    }
    // Contents mode remaps regular files after discovering their type.
    for task in &tasks {
        for other in &tasks {
            if task.destination.starts_with(&other.source)
                || other.source.starts_with(&task.destination)
            {
                return Err(invalid(task.root, &task.source));
            }
        }
    }
    Ok(tasks)
}
fn placeholder() -> Attrs {
    vfsi_core::metadata_from_attrs(vfsi_core::VfAttrs::default())
}
fn ensure_dirs(fs: &impl Vfsi, tasks: &mut [Task]) -> Result<()> {
    if tasks.is_empty() {
        return Ok(());
    }
    let paths: Vec<_> = tasks
        .iter()
        .map(|t| crate::MkDirOp::new(&t.destination, 0o777))
        .collect();
    match fs.vmkdir(&paths) {
        Ok(()) => {
            for task in tasks {
                task.fresh_destination = true;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Semantic EEXIST may follow creations. Reconcile each object; never
            // infer a committed prefix or replay an ambiguous transport error.
            for task in tasks {
                match fs.symlink_attrs(&task.destination) {
                    Ok(meta) if meta.is_dir() => {
                        task.fresh_destination = false;
                    }
                    Ok(_) => return Err(invalid(task.root, &task.destination)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        fs.vmkdir(&[crate::MkDirOp::new(&task.destination, 0o777)])
                            .map_err(|e| mapped(e, std::slice::from_ref(task)))?;
                        task.fresh_destination = true;
                    }
                    Err(e) => return Err(mapped(e, std::slice::from_ref(task))),
                }
            }
            Ok(())
        }
        Err(e) => Err(mapped(e, tasks)),
    }
}
fn run<P: AsRef<Path>>(
    fs: &impl Vfsi,
    sources: &[P],
    destination: &Path,
    options: CopyOptions,
    exact: bool,
    mut progress: Progress<'_>,
) -> Result<TransferSummary> {
    let limits = fs.limits();
    let mut budget = Budget {
        entries: 0,
        bytes: 0,
        max_entries: options
            .max_entries
            .unwrap_or(limits.directory_entry_limit()),
        max_bytes: options
            .max_path_bytes
            .unwrap_or(limits.directory_path_byte_limit()),
    };
    let mut roots = roots(fs, sources, destination, options, exact, &mut budget)?;
    let mut summary = TransferSummary::default();
    if roots.is_empty() {
        return Ok(summary);
    }
    // The container must exist or be creatable; its parent must already exist.
    if !exact {
        let container = Task {
            source: roots[0].source.clone(),
            destination: normalize(destination, budget.max_bytes)?,
            metadata: roots[0].metadata.clone(),
            size: 0,
            root: 0,
            depth: 0,
            fresh_destination: false,
        };
        let mut containers = [container];
        ensure_dirs(fs, &mut containers)?;
        for task in &mut roots {
            task.fresh_destination = containers[0].fresh_destination;
        }
    }
    copy_roots(
        fs,
        roots,
        options,
        &mut budget,
        &mut summary,
        &mut progress,
        &mut HashSet::new(),
    )?;
    Ok(summary)
}
fn copy_roots(
    fs: &impl Vfsi,
    roots: Vec<Task>,
    options: CopyOptions,
    budget: &mut Budget,
    summary: &mut TransferSummary,
    progress: &mut Progress<'_>,
    skipped: &mut HashSet<usize>,
) -> Result<()> {
    let limits = fs.limits();
    let mut pending = Vec::new();
    for batch in roots.chunks(options.batch_size) {
        dispatch(
            fs,
            batch.to_vec(),
            &mut pending,
            options,
            summary,
            progress,
            skipped,
        )?;
        if summary.stopped {
            return Ok(());
        }
    }
    while !pending.is_empty() && !summary.stopped {
        let wave_size = pending.len().min(options.batch_size);
        let wave: Vec<_> = pending.drain(pending.len() - wave_size..).collect();
        let paths: Vec<_> = wave.iter().map(|t| &t.source).collect();
        let remaining = budget.max_entries.saturating_sub(budget.entries);
        let options_visit = ListDirOptions::new()
            .fields(fields())
            .max_entries(remaining)
            .max_path_bytes(budget.max_bytes.saturating_sub(budget.bytes));
        let mut callback_error = None;
        let completed = fs
            .vlistdirs(&paths, options_visit, |index, page| {
                let result = (|| {
                    let parent = wave
                        .get(index)
                        .ok_or_else(|| contract("transfer: invalid visitor index"))?;
                    if page.path != parent.source {
                        return Err(contract("transfer: misassociated directory page"));
                    }
                    let mut children = Vec::new();
                    for entry in page.entries {
                        if entry.path().parent() != Some(parent.source.as_path()) {
                            return Err(contract("transfer: invalid child path"));
                        }
                        let name = entry
                            .file_name()
                            .ok_or_else(|| contract("transfer: missing child name"))?;
                        let dest = parent.destination.join(name);
                        budget.charge(entry.path(), &dest, parent.root)?;
                        let depth = parent
                            .depth
                            .checked_add(1)
                            .ok_or_else(|| limit(parent.root, entry.path()))?;
                        if entry.attrs().is_dir()
                            && depth > options.depth.map_or(limits.walk_depth_limit(), |d| d.get())
                        {
                            if options.depth.is_some() {
                                summary.entries_skipped += 1;
                                skipped.insert(parent.root);
                                continue;
                            }
                            return Err(limit(parent.root, entry.path()));
                        }
                        children.push(Task {
                            source: entry.path().to_path_buf(),
                            destination: dest,
                            metadata: entry.attrs().clone(),
                            size: 0,
                            root: parent.root,
                            depth,
                            fresh_destination: parent.fresh_destination,
                        });
                        if children.len() == options.batch_size {
                            dispatch(
                                fs,
                                std::mem::take(&mut children),
                                &mut pending,
                                options,
                                summary,
                                progress,
                                skipped,
                            )?;
                            if summary.stopped {
                                return Ok(ControlFlow::Break(()));
                            }
                        }
                    }
                    dispatch(
                        fs,
                        children,
                        &mut pending,
                        options,
                        summary,
                        progress,
                        skipped,
                    )?;
                    Ok(if summary.stopped {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    })
                })();
                if let Err(error) = &result {
                    callback_error = Some(error.clone());
                }
                result
            })
            .map_err(|e| callback_error.take().unwrap_or_else(|| mapped(e, &wave)))?;
        if completed.len() > wave.len()
            || (!summary.stopped
                && (completed.len() != wave.len()
                    || completed
                        .iter()
                        .any(|c| *c != crate::TraversalCompletion::Complete)))
        {
            return Err(contract("transfer: incomplete directory traversal"));
        }
    }
    Ok(())
}
fn dispatch(
    fs: &impl Vfsi,
    tasks: Vec<Task>,
    pending: &mut Vec<Task>,
    options: CopyOptions,
    summary: &mut TransferSummary,
    progress: &mut Progress<'_>,
    skipped: &mut HashSet<usize>,
) -> Result<()> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for mut task in tasks {
        if task.metadata.is_dir() {
            dirs.push(task);
        } else if task.metadata.is_file() {
            task.size = task.metadata.len().ok_or_else(|| {
                Error::unsupported(task.root).with_context("transfer size", &task.source)
            })?;
            files.push(task);
        } else if options.unsupported == UnsupportedEntry::Skip {
            summary.entries_skipped += 1;
            skipped.insert(task.root);
        } else {
            return Err(Error::client(task.root, libc::ENOTSUP as u32)
                .with_context("transfer", &task.source));
        }
    }
    ensure_dirs(fs, &mut dirs)?;
    summary.directories_processed += dirs.len() as u64;
    if let Some(callback) = progress.as_mut() {
        for task in &dirs {
            if callback(TransferProgress {
                source: &task.source,
                destination: &task.destination,
                file_bytes_copied: 0,
                file_size: 0,
                summary: *summary,
            })?
            .is_break()
            {
                summary.stopped = true;
                return Ok(());
            }
        }
    }
    pending.extend(dirs);
    let chunk = options.chunk_bytes.min(fs.limits().read_byte_limit());
    let native_copy = progress.is_none()
        && options.existing == Existing::Replace
        && !options.flags.preserve_permissions();
    let mut start = 0;
    while start < files.len() {
        let mut end = start;
        let mut bytes = 0;
        while end < files.len() && end - start < options.batch_size {
            let required = chunk.min(usize::try_from(files[end].size).unwrap_or(usize::MAX));
            if !native_copy && required > fs.limits().read_byte_limit() - bytes {
                break;
            }
            if !native_copy {
                bytes += required;
            }
            end += 1;
        }
        copy_batch(
            fs,
            &files[start..end],
            options,
            chunk,
            summary,
            progress,
            skipped,
        )?;
        if summary.stopped {
            break;
        }
        start = end;
    }
    Ok(())
}
fn copy_batch(
    fs: &impl Vfsi,
    tasks: &[Task],
    options: CopyOptions,
    chunk: usize,
    summary: &mut TransferSummary,
    progress: &mut Progress<'_>,
    skipped: &mut HashSet<usize>,
) -> Result<()> {
    if options.flags.preserve_permissions() {
        for task in tasks {
            if task.metadata.mode().is_none() {
                return Err(Error::client(task.root, vfsi_core::VF_ERR_UNSUPPORTED)
                    .with_context("transfer permissions", &task.source));
            }
        }
    }
    if progress.is_none()
        && options.existing == Existing::Replace
        && !options.flags.preserve_permissions()
    {
        prepare_replace(fs, tasks)?;
        let requests: Vec<_> = tasks
            .iter()
            .map(|t| {
                OpenOp::new(&t.destination, OpenFlags::WRITE | OpenFlags::CREATE_NEW).mode(0o600)
            })
            .collect();
        let handles = fs.vopen(&requests).map_err(|e| mapped(e, tasks))?;
        let valid = handles.len() == tasks.len();
        let close = fs.close_files(handles).map_err(|e| mapped(e, tasks));
        if !valid {
            return Err(contract(
                "transfer: invalid native COPY reservation cardinality",
            ));
        }
        close?;
        let pairs: Vec<_> = tasks.iter().map(|t| (&t.source, &t.destination)).collect();
        fs.vcopy(&pairs, CopyOption::default())
            .map_err(|e| mapped(e, tasks))?;
        summary.files_copied += tasks.len() as u64;
        summary.bytes_copied = None;
        return Ok(());
    }
    let requests: Vec<_> = tasks
        .iter()
        .map(|t| OpenOp::new(&t.source, OpenFlags::READ))
        .collect();
    let sources = fs.vopen(&requests).map_err(|e| mapped(e, tasks))?;
    let mut destinations = Vec::new();
    let mut selected = Vec::new();
    let operation = (|| {
        if sources.len() != tasks.len() {
            return Err(contract("transfer: invalid source OPEN cardinality"));
        }
        if options.existing == Existing::Skip {
            // A failed batch OPEN returns no handles but may have created a
            // prefix. Singleton exclusive reservations are necessary to safely
            // distinguish skipped files without guessing ownership of that prefix.
            for (index, task) in tasks.iter().enumerate() {
                let request =
                    OpenOp::new(&task.destination, OpenFlags::WRITE | OpenFlags::CREATE_NEW)
                        .mode(0o600);
                match fs.vopen(&[request]) {
                    Ok(mut files) => {
                        if files.len() != 1 {
                            let _ = fs.close_files(files);
                            return Err(contract("transfer: invalid destination OPEN cardinality"));
                        }
                        destinations.push(files.remove(0));
                        selected.push(index);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        summary.entries_skipped += 1;
                        skipped.insert(task.root);
                    }
                    Err(e) => return Err(mapped(e, std::slice::from_ref(task))),
                }
            }
        } else {
            if options.existing == Existing::Replace {
                prepare_replace(fs, tasks)?;
            }
            let flags = OpenFlags::WRITE | OpenFlags::CREATE_NEW;
            let requests: Vec<_> = tasks
                .iter()
                .map(|t| OpenOp::new(&t.destination, flags).mode(0o600))
                .collect();
            destinations = fs.vopen(&requests).map_err(|e| mapped(e, tasks))?;
            if destinations.len() != tasks.len() {
                return Err(contract("transfer: invalid destination OPEN cardinality"));
            }
            selected.extend(0..tasks.len());
        }
        let mut offsets = vec![0u64; selected.len()];
        let mut done = vec![false; selected.len()];
        let mut buffers: Vec<_> = selected
            .iter()
            .map(|i| vec![0u8; chunk.min(usize::try_from(tasks[*i].size).unwrap_or(usize::MAX))])
            .collect();
        while done.iter().any(|v| !v) {
            let active: Vec<_> = (0..selected.len()).filter(|i| !done[*i]).collect();
            let reads: Vec<_> = buffers
                .iter_mut()
                .enumerate()
                .filter(|(i, _)| !done[*i])
                .map(|(i, buffer)| {
                    let remaining = tasks[selected[i]].size.saturating_sub(offsets[i]);
                    let length = buffer
                        .len()
                        .min(usize::try_from(remaining).unwrap_or(usize::MAX));
                    ReadOp::into(&sources[selected[i]], offsets[i], &mut buffer[..length])
                })
                .collect();
            let read = fs
                .vread(reads, ReadOptions::new())
                .map_err(|e| active_error(e, &active, &selected, tasks))?;
            if read.len() != active.len() {
                return Err(contract("transfer: invalid READ cardinality"));
            }
            for (result, i) in read.iter().zip(&active) {
                let remaining = tasks[selected[*i]].size.saturating_sub(offsets[*i]);
                if result.offset() != offsets[*i]
                    || result.data().is_some()
                    || result.read()
                        > buffers[*i]
                            .len()
                            .min(usize::try_from(remaining).unwrap_or(usize::MAX))
                {
                    return Err(contract("transfer: misassociated READ result"));
                }
                if result.read() == 0 && !result.eof() && remaining != 0 {
                    return Err(contract("transfer: READ made no progress"));
                }
            }
            let writes: Vec<_> = read
                .iter()
                .zip(&active)
                .map(|(r, i)| WriteOp::at(&destinations[*i], offsets[*i], &buffers[*i][..r.read()]))
                .collect();
            let written = fs
                .vwrite(&writes, WriteOptions::new().write_all(true))
                .map_err(|e| active_error(e, &active, &selected, tasks))?;
            if written.len() != read.len() {
                return Err(contract("transfer: invalid WRITE cardinality"));
            }
            for ((written, read), i) in written.iter().zip(&read).zip(&active) {
                if written.offset != offsets[*i] || written.written != read.read() {
                    return Err(contract("transfer: invalid complete WRITE progress"));
                }
                offsets[*i] = offsets[*i]
                    .checked_add(read.read() as u64)
                    .ok_or_else(|| limit(tasks[selected[*i]].root, &tasks[selected[*i]].source))?;
                if let Some(bytes) = &mut summary.bytes_copied {
                    *bytes = bytes.checked_add(read.read() as u64).ok_or_else(|| {
                        limit(tasks[selected[*i]].root, &tasks[selected[*i]].source)
                    })?;
                }
                done[*i] = read.eof() || offsets[*i] == tasks[selected[*i]].size;
                if done[*i] {
                    summary.files_copied += 1;
                }
            }
            if let Some(callback) = progress.as_mut() {
                for i in &active {
                    let task = &tasks[selected[*i]];
                    if callback(TransferProgress {
                        source: &task.source,
                        destination: &task.destination,
                        file_bytes_copied: offsets[*i],
                        file_size: task.size,
                        summary: *summary,
                    })?
                    .is_break()
                    {
                        summary.stopped = true;
                        break;
                    }
                }
            }
            if summary.stopped {
                break;
            }
        }
        if options.flags.preserve_permissions() {
            // Finalize completed handles together; cancellation excludes partial files.
            let (updates, completed): (Vec<_>, Vec<_>) = destinations
                .iter()
                .enumerate()
                .filter(|(i, _)| done[*i])
                .map(|(i, file)| {
                    let task = &tasks[selected[i]];
                    (
                        SetAttrsOp::file(file).permissions(
                            task.metadata
                                .permissions()
                                .expect("MODE validated before copy"),
                        ),
                        task,
                    )
                })
                .unzip();
            fs.vsetattrs(&updates).map_err(|e| mapped(e, &completed))?;
        }
        Ok(())
    })();
    let close_sources = fs.close_files(sources).map_err(|e| mapped(e, tasks));
    let close_destinations = fs.close_files(destinations).map_err(|e| {
        if let Some(task) = e
            .index()
            .and_then(|i| selected.get(i))
            .and_then(|i| tasks.get(*i))
        {
            e.with_index(task.root)
                .with_context("transfer", &task.destination)
        } else {
            e
        }
    });
    operation?;
    close_sources?;
    close_destinations
}
fn active_error(error: Error, active: &[usize], selected: &[usize], tasks: &[Task]) -> Error {
    if let Some(task) = error
        .index()
        .and_then(|i| active.get(i))
        .and_then(|i| selected.get(*i))
        .and_then(|i| tasks.get(*i))
    {
        error
            .with_index(task.root)
            .with_context("transfer", &task.source)
    } else {
        error
    }
}
fn same_file(source: &Attrs, destination: &Attrs) -> bool {
    // Conservative without a portable filesystem-id field: matching file IDs
    // are rejected, even if two mounted filesystems could reuse the same ID.
    source.file_id().is_some() && source.file_id() == destination.file_id()
}
fn prepare_replace(fs: &impl Vfsi, tasks: &[Task]) -> Result<()> {
    let mut existing = Vec::new();
    for task in tasks {
        if task.fresh_destination {
            continue;
        }
        match fs.symlink_attrs(&task.destination) {
            Ok(meta) if !meta.is_file() || same_file(&task.metadata, &meta) => {
                return Err(invalid(task.root, &task.destination));
            }
            Ok(_) => existing.push(task.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(mapped(e, std::slice::from_ref(task))),
        }
    }
    if !existing.is_empty() {
        let paths: Vec<_> = existing.iter().map(|t| &t.destination).collect();
        fs.vremove(
            &paths,
            crate::RemoveMode::Entry,
            crate::RemoveOptions::new(),
        )
        .map_err(|e| mapped(e, &existing))?;
    }
    Ok(())
}

/// Move roots within the client's namespace. Rename is attempted only for
/// Container+Replace without progress when the destination is absent. Only an
/// explicit cross-device error triggers copy/delete fallback. Other modes copy
/// first; source deletion occurs only after complete copying and successful
/// explicit CLOSE, with no skipped entries. Cancellation/errors retain sources.
/// Stable/quiescent source trees are required: this is not a snapshot or a
/// transactional move. An error deleting a source can leave both copies.
pub fn move_items<P: AsRef<Path>>(
    fs: &impl Vfsi,
    sources: &[P],
    destination: impl AsRef<Path>,
    options: CopyOptions,
) -> Result<TransferSummary> {
    move_run(fs, sources, destination.as_ref(), options, None)
}
pub fn move_items_with_progress<P: AsRef<Path>>(
    fs: &impl Vfsi,
    sources: &[P],
    destination: impl AsRef<Path>,
    options: CopyOptions,
    mut progress: impl FnMut(TransferProgress<'_>) -> Result<ControlFlow<()>>,
) -> Result<TransferSummary> {
    move_run(
        fs,
        sources,
        destination.as_ref(),
        options,
        Some(&mut progress),
    )
}
fn move_run<P: AsRef<Path>>(
    fs: &impl Vfsi,
    sources: &[P],
    destination: &Path,
    options: CopyOptions,
    mut progress: Progress<'_>,
) -> Result<TransferSummary> {
    let limits = fs.limits();
    let mut budget = Budget {
        entries: 0,
        bytes: 0,
        max_entries: options
            .max_entries
            .unwrap_or(limits.directory_entry_limit()),
        max_bytes: options
            .max_path_bytes
            .unwrap_or(limits.directory_path_byte_limit()),
    };
    // Validate all roots/overlaps before the first mutation.
    let mut tasks = roots(fs, sources, destination, options, false, &mut budget)?;
    let mut summary = TransferSummary::default();
    if let Some(task) = tasks.first() {
        let container = Task {
            source: task.source.clone(),
            destination: normalize(destination, budget.max_bytes)?,
            metadata: task.metadata.clone(),
            size: 0,
            root: 0,
            depth: 0,
            fresh_destination: false,
        };
        let mut containers = [container];
        ensure_dirs(fs, &mut containers)?;
        for task in &mut tasks {
            task.fresh_destination = containers[0].fresh_destination;
        }
    }
    // Contents operands can share destinations. Preserve their root ordering;
    // Container roots were preflighted as independent and can be vectorized.
    let cohort = if options.layout == CopyLayout::Contents {
        1
    } else {
        options.batch_size
    };
    for batch in tasks.chunks(cohort) {
        let mut copies = Vec::new();
        let mut renames = Vec::new();
        for task in batch {
            if !task.metadata.is_file() && !task.metadata.is_dir() {
                summary.entries_skipped += 1;
                continue;
            }
            let can_rename = progress.is_none()
                && options.layout == CopyLayout::Container
                && options.existing == Existing::Replace
                && options.depth.is_none();
            if can_rename {
                renames.push(task.clone());
            } else {
                copies.push(task.clone());
            }
        }
        if !renames.is_empty() {
            let pairs: Vec<_> = renames
                .iter()
                .map(|task| (&task.source, &task.destination))
                .collect();
            match fs.vrename(&pairs, crate::RenameOptions::NoReplace) {
                Ok(()) => summary.roots_renamed += renames.len() as u64,
                Err(error) if !error.is_transport() => {
                    // A strict batch may have renamed a prefix. Do not infer that
                    // prefix from its index or replay any rename. Reconcile paths
                    // before copying the confirmed untouched operands instead.
                    let can_fallback = matches!(
                        error.kind(),
                        std::io::ErrorKind::CrossesDevices
                            | std::io::ErrorKind::AlreadyExists
                            | std::io::ErrorKind::Unsupported
                    );
                    reconcile_renames(
                        fs,
                        &renames,
                        error.clone(),
                        can_fallback,
                        &mut copies,
                        &mut summary,
                    )?;
                    if !can_fallback {
                        return Err(mapped(error, &renames));
                    }
                }
                Err(error) => return Err(mapped(error, &renames)),
            }
        }
        copies.sort_by_key(|task| task.root);
        let mut skipped = HashSet::new();
        copy_roots(
            fs,
            copies.clone(),
            options,
            &mut budget,
            &mut summary,
            &mut progress,
            &mut skipped,
        )?;
        // On error or cancellation retain all copied sources in this wave.
        // Only complete, successfully closed, unskipped roots are delete-eligible.
        if summary.stopped {
            break;
        }
        let complete: Vec<_> = copies
            .into_iter()
            .filter(|task| !skipped.contains(&task.root))
            .collect();
        if !complete.is_empty() {
            let paths: Vec<_> = complete.iter().map(|task| &task.source).collect();
            fs.vremove(&paths, crate::RemoveMode::Tree, crate::RemoveOptions::new())
                .map_err(|error| mapped(error, &complete))?;
            summary.roots_removed += complete.len() as u64;
        }
    }
    Ok(summary)
}

fn reconcile_renames(
    fs: &impl Vfsi,
    tasks: &[Task],
    failure: Error,
    allow_fallback: bool,
    copies: &mut Vec<Task>,
    summary: &mut TransferSummary,
) -> Result<()> {
    for task in tasks {
        match fs.symlink_attrs(&task.source) {
            Ok(source) => {
                if task.metadata.file_id().is_some() && !same_file(&task.metadata, &source) {
                    return Err(invalid(task.root, &task.source));
                }
                match fs.symlink_attrs(&task.destination) {
                    Err(error)
                        if error.kind() == std::io::ErrorKind::NotFound && allow_fallback =>
                    {
                        copies.push(task.clone())
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Err(mapped(failure.clone(), tasks));
                    }
                    Err(error) => return Err(mapped(error, std::slice::from_ref(task))),
                    Ok(target) if allow_fallback && !same_file(&task.metadata, &target) => {
                        copies.push(task.clone())
                    }
                    Ok(_) => return Err(mapped(failure.clone(), tasks)),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let target = fs
                    .symlink_attrs(&task.destination)
                    .map_err(|error| mapped(error, std::slice::from_ref(task)))?;
                if !same_file(&task.metadata, &target) {
                    return Err(mapped(failure.clone(), tasks));
                }
                summary.roots_renamed += 1;
            }
            Err(error) => return Err(mapped(error, std::slice::from_ref(task))),
        }
    }
    Ok(())
}
