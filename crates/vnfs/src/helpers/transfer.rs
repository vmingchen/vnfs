use crate::{
    Attributes, Attrs, AttrsOptions, ControlFlow, CopyOptions, DepthLimit, Error, ListDirOptions,
    OpenFlags, OpenOp, ReadOp, ReadOptions, SetAttrsOp, Vfsi, VfsiExt, WriteOp, WriteOptions,
};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use vfsi_core::IndexedError;
use vfsi_core::internal::{ObservedObject, TraversalBudget};
type Result<T> = std::result::Result<T, IndexedError>;

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
/// ```compile_fail
/// use vnfs::helpers::TransferOptions;
/// TransferOptions::new().with_batch_size(0);
/// ```
/// ```compile_fail
/// use vnfs::helpers::TransferOptions;
/// TransferOptions::new().with_chunk_size(0);
/// ```
#[derive(Clone, Copy, Debug)]
#[must_use = "configuration and requests have no effect until used"]
pub struct TransferOptions {
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
impl Default for TransferOptions {
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
impl TransferOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_existing(mut self, value: Existing) -> Self {
        self.existing = value;
        self
    }
    pub fn with_layout(mut self, value: CopyLayout) -> Self {
        self.layout = value;
        self
    }
    pub fn with_unsupported_entries(mut self, value: UnsupportedEntry) -> Self {
        self.unsupported = value;
        self
    }
    /// Preserve file permission bits, not directory modes, ownership, times or ACLs.
    pub fn with_preserve_permissions(mut self, value: bool) -> Self {
        self.flags.set_preserve_permissions(value);
        self
    }
    pub fn with_batch_size(mut self, value: std::num::NonZeroUsize) -> Self {
        self.batch_size = value.get();
        self
    }
    pub fn with_chunk_size(mut self, value: std::num::NonZeroUsize) -> Self {
        self.chunk_bytes = value.get();
        self
    }
    pub fn with_max_entries(mut self, value: usize) -> Self {
        self.max_entries = Some(value);
        self
    }
    /// Aggregate source+destination logical path bytes over the whole transfer.
    pub fn with_max_path_bytes(mut self, value: usize) -> Self {
        self.max_path_bytes = Some(value);
        self
    }
    /// Deliberately omit deeper directories. Omitted subtrees count as skipped;
    /// move helpers then retain the source. Without this override, exceeding the
    /// client's depth limit is an error rather than successful truncation.
    pub fn with_max_depth(mut self, value: DepthLimit) -> Self {
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
type Progress<'a> =
    Option<&'a mut dyn FnMut(TransferProgress<'_>) -> std::io::Result<ControlFlow<()>>>;

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
    options: TransferOptions,
) -> Result<TransferSummary> {
    run(fs, sources, destination.as_ref(), options, false, None)
}
pub fn copy_items_with_progress<P: AsRef<Path>>(
    fs: &impl Vfsi,
    sources: &[P],
    destination: impl AsRef<Path>,
    options: TransferOptions,
    mut progress: impl FnMut(TransferProgress<'_>) -> std::io::Result<ControlFlow<()>>,
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
    options: TransferOptions,
) -> std::io::Result<TransferSummary> {
    run(fs, &[source], destination.as_ref(), options, true, None).map_err(IndexedError::into_cause)
}
pub fn copy_tree_with_progress(
    fs: &impl Vfsi,
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    options: TransferOptions,
    mut progress: impl FnMut(TransferProgress<'_>) -> std::io::Result<ControlFlow<()>>,
) -> std::io::Result<TransferSummary> {
    run(
        fs,
        &[source],
        destination.as_ref(),
        options,
        true,
        Some(&mut progress),
    )
    .map_err(IndexedError::into_cause)
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
    descriptor_copy: bool,
}
fn invalid_cause(path: &Path) -> Error {
    vfsi_core::internal::errors::with_context(
        std::io::ErrorKind::InvalidInput.into(),
        "transfer",
        path,
    )
}
fn invalid(root: usize, path: &Path) -> IndexedError {
    IndexedError::at(root, invalid_cause(path))
}
fn limit(root: usize, path: &Path) -> IndexedError {
    IndexedError::at(
        root,
        vfsi_core::internal::errors::with_context(
            std::io::Error::from_raw_os_error(libc::EFBIG),
            "transfer",
            path,
        ),
    )
}
fn contract(message: &'static str) -> IndexedError {
    IndexedError::unattributed(vfsi_core::internal::errors::invalid_reply(message))
}
fn normalize(path: &Path, max: usize) -> std::io::Result<PathBuf> {
    if path.as_os_str().len() > max {
        return Err(std::io::Error::from_raw_os_error(libc::EFBIG));
    }
    if path.as_os_str().is_empty() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(invalid_cause(path));
    }
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::RootDir | Component::CurDir => {}
            _ => return Err(invalid_cause(path)),
        }
    }
    Ok(out)
}

fn fields() -> Attributes {
    Attributes::MODE | Attributes::SIZE | Attributes::identity()
}
fn mapped(error: IndexedError, tasks: &[impl std::borrow::Borrow<Task>]) -> IndexedError {
    match error.index() {
        Some(index) => match tasks.get(index).map(std::borrow::Borrow::borrow) {
            Some(task) => error
                .map_index(|_| task.root)
                .with_context("transfer", &task.source),
            None => contract("transfer: invalid batch error index"),
        },
        None => error,
    }
}

fn roots<P: AsRef<Path>>(
    fs: &impl Vfsi,
    sources: &[P],
    destination: &Path,
    options: TransferOptions,
    exact: bool,
    budget: &mut TraversalBudget,
    max_path_bytes: usize,
) -> Result<Vec<Task>> {
    if sources.len() > budget.remaining_entries() {
        return Err(limit(0, destination));
    }
    let destination = normalize(destination, max_path_bytes).map_err(IndexedError::unattributed)?;
    let mut tasks = Vec::with_capacity(sources.len());
    for (root, source) in sources.iter().enumerate() {
        let source =
            normalize(source.as_ref(), max_path_bytes).map_err(|e| IndexedError::at(root, e))?;
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
        budget
            .charge_paths([source.as_path(), dest.as_path()])
            .map_err(|_| limit(root, &source))?;
        // Query one bounded root wave, without materializing a whole tree.
        tasks.push(Task {
            source,
            destination: dest,
            metadata: placeholder(),
            size: 0,
            root,
            depth: 0,
            fresh_destination: false,
            descriptor_copy: false,
        });
    }
    for batch in tasks.chunks_mut(options.batch_size) {
        let paths: Vec<_> = batch.iter().map(|t| &t.source).collect();
        let metadata = fs
            .getattrs_all(
                &paths,
                AttrsOptions::new()
                    .with_fields(fields())
                    .with_follow_final_symlink(false),
            )
            .map_err(|e| mapped(e, batch))?;
        if metadata.len() != batch.len() {
            return Err(contract("transfer: invalid metadata cardinality"));
        }
        for (task, metadata) in batch.iter_mut().zip(metadata) {
            if exact && !metadata.is_dir() {
                return Err(IndexedError::at(
                    task.root,
                    vfsi_core::internal::errors::with_context(
                        std::io::Error::from_raw_os_error(libc::ENOTDIR),
                        "transfer",
                        &task.source,
                    ),
                ));
            }
            if !metadata.is_dir()
                && !metadata.is_file()
                && options.unsupported == UnsupportedEntry::Error
            {
                return Err(IndexedError::at(
                    task.root,
                    vfsi_core::internal::errors::with_context(
                        std::io::Error::from_raw_os_error(libc::ENOTSUP),
                        "transfer",
                        &task.source,
                    ),
                ));
            }
            if options.layout == CopyLayout::Contents && metadata.is_file() && !exact {
                let mapped = destination.join(
                    task.source
                        .file_name()
                        .ok_or_else(|| invalid(task.root, &task.source))?,
                );
                let additional_bytes = mapped
                    .as_os_str()
                    .len()
                    .saturating_sub(task.destination.as_os_str().len());
                budget
                    .charge_bytes(additional_bytes)
                    .map_err(|_| limit(task.root, &task.source))?;
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
    reject_aliased_overlap(fs, &tasks, options, max_path_bytes)?;
    Ok(tasks)
}

fn reject_aliased_overlap(
    fs: &impl Vfsi,
    tasks: &[Task],
    options: TransferOptions,
    max_path_bytes: usize,
) -> Result<()> {
    let sources: Vec<_> = tasks
        .iter()
        .filter(|task| task.metadata.is_dir() || task.metadata.is_file())
        .collect();
    let directories: Vec<_> = sources
        .iter()
        .copied()
        .filter(|task| task.metadata.is_dir())
        .collect();
    let check_files = options.layout == CopyLayout::Contents
        && options.existing == Existing::Replace
        && sources.len() > 1;
    if directories.is_empty() && !check_files {
        return Ok(());
    }
    // Borrow each prefix from its already budgeted operand path. Owning
    // every prefix would make a deep path consume quadratic path storage.
    let entries = read_transfer_ancestry(
        fs,
        sources
            .iter()
            .filter(|task| task.metadata.is_dir() || check_files)
            .map(|task| (task.root, task.destination.as_path())),
        false,
        options.batch_size,
        HashMap::new(),
    )?;
    if let Some(task) = sources.iter().find(|task| {
        task.metadata.is_file()
            && entries
                .get(task.destination.as_path())
                .and_then(Option::as_ref)
                .is_some_and(Attrs::is_dir)
    }) {
        return Err(invalid(task.root, &task.destination));
    }
    let mut resolved = HashMap::new();
    let followed = if directories.is_empty() {
        HashMap::new()
    } else {
        let operands = || {
            sources.iter().flat_map(|task| {
                [&task.destination, &task.source]
                    .into_iter()
                    .map(|path| (task.root, path.as_path()))
            })
        };
        let mut nofollow = read_transfer_ancestry(
            fs,
            operands()
                .flat_map(|(root, path)| path.ancestors().skip(1).map(move |path| (root, path))),
            false,
            options.batch_size,
            HashMap::new(),
        )?;
        let mut projected_bytes = 0usize;
        for (root, path) in operands() {
            if resolved.contains_key(path) {
                continue;
            }
            if let Some(projected) =
                resolve_transfer_ancestor_alias(fs, root, path, &nofollow, max_path_bytes)?
            {
                projected_bytes = projected_bytes.saturating_add(projected.as_os_str().len());
                if projected_bytes > max_path_bytes {
                    return Err(limit(root, path));
                }
                resolved.insert(path, projected);
            }
        }
        // A no-follow result for a non-link is also the followed result.
        nofollow.retain(|_, attrs| !attrs.as_ref().is_some_and(Attrs::is_symlink));
        read_transfer_ancestry(
            fs,
            operands().flat_map(|(root, path)| {
                transfer_ancestors(path, &resolved).map(move |path| (root, path))
            }),
            true,
            options.batch_size,
            nofollow,
        )?
    };
    let mut checked_regular_targets = HashSet::new();
    for &task in &sources {
        if !directories.is_empty() {
            let ancestors = transfer_ancestors(&task.destination, &resolved).filter_map(|path| {
                followed[path]
                    .as_ref()
                    .filter(|attrs| attrs.is_dir())
                    .map(|attrs| (path, attrs))
            });
            let pairs = ancestors.flat_map(|destination| {
                directories
                    .iter()
                    .map(move |source| ((source.source.as_path(), &source.metadata), destination))
            });
            reject_identity_pairs(fs, task, pairs, options.batch_size)?;
        }
        if let Some(attrs) = entries
            .get(task.destination.as_path())
            .and_then(Option::as_ref)
            && attrs.is_dir()
        {
            let destination = (task.destination.as_path(), attrs);
            let pairs = directories
                .iter()
                .map(|source| ((source.source.as_path(), &source.metadata), destination));
            reject_identity_pairs(fs, task, pairs, options.batch_size)?;
            let pairs = sources
                .iter()
                .flat_map(|source| transfer_ancestors(&source.source, &resolved))
                .filter_map(|path| {
                    followed[path]
                        .as_ref()
                        .map(|attrs| ((path, attrs), destination))
                });
            reject_identity_pairs(fs, task, pairs, options.batch_size)?;
        }
        if let Some(attrs) = entries
            .get(task.destination.as_path())
            .and_then(Option::as_ref)
            && attrs.is_file()
            && checked_regular_targets.insert(task.destination.as_path())
        {
            let destination = (task.destination.as_path(), attrs);
            let pairs = sources
                .iter()
                .filter(|source| {
                    source.metadata.is_file()
                        && match (source.metadata.file_id(), attrs.file_id()) {
                            (Some(left), Some(right)) if left != right => {
                                source.source.file_name() == task.destination.file_name()
                            }
                            _ => true,
                        }
                })
                .map(|source| ((source.source.as_path(), &source.metadata), destination));
            reject_identity_pairs(fs, task, pairs, options.batch_size)?;
        }
    }
    Ok(())
}

fn transfer_ancestors<'a>(
    path: &'a Path,
    resolved: &'a HashMap<&Path, PathBuf>,
) -> impl Iterator<Item = &'a Path> {
    path.ancestors().skip(1).chain(
        resolved
            .get(path)
            .into_iter()
            .flat_map(|alias| alias.ancestors().skip(1)),
    )
}

fn resolve_transfer_ancestor_alias(
    fs: &impl Vfsi,
    root: usize,
    original: &Path,
    observed: &HashMap<&Path, Option<Attrs>>,
    max_path_bytes: usize,
) -> Result<Option<PathBuf>> {
    let Some(first) = original
        .ancestors()
        .skip(1)
        .filter(|path| {
            observed
                .get(path)
                .and_then(Option::as_ref)
                .is_some_and(Attrs::is_symlink)
        })
        .last()
    else {
        return Ok(None);
    };
    let mut path = original.to_path_buf();
    let mut link = first.to_path_buf();
    let attributed =
        |error| IndexedError::at(root, error).with_context("transfer ancestry", original);
    for _ in 0..40 {
        let target = fs.read_link(&link).map_err(attributed)?;
        let mut resolved = if target.is_absolute() {
            PathBuf::from("/")
        } else {
            link.parent().unwrap_or(Path::new("/")).to_path_buf()
        };
        let suffix = path.strip_prefix(&link).expect("alias is a path prefix");
        let mut checked_bytes = resolved.as_os_str().len();
        for component in target.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => resolved.push(name),
                Component::ParentDir => {
                    // A kernel lookup can follow a component before a parent
                    // step that native NFS instead folds lexically. Only
                    // collapse it in the kernel view with proven ancestry.
                    if !fs.normalizes_symlink_targets_lexically()
                        && resolved
                            .ancestors()
                            .take_while(|path| path.as_os_str().len() > checked_bytes)
                            .any(|prefix| {
                                !fs.symlink_attrs(prefix).is_ok_and(|attrs| attrs.is_dir())
                            })
                    {
                        return Err(attributed(std::io::Error::new(
                            std::io::ErrorKind::Unsupported,
                            "cannot verify directory ancestry through this symlink target",
                        )));
                    }
                    resolved.pop();
                    checked_bytes = resolved.as_os_str().len();
                }
                Component::Prefix(_) => {
                    return Err(attributed(std::io::ErrorKind::Unsupported.into()));
                }
            }
            if resolved.as_os_str().len() > max_path_bytes {
                return Err(limit(root, original));
            }
        }
        if resolved
            .as_os_str()
            .len()
            .saturating_add(suffix.as_os_str().len())
            > max_path_bytes
        {
            return Err(limit(root, original));
        }
        resolved.push(suffix);
        if resolved.as_os_str().len() > max_path_bytes {
            return Err(limit(root, original));
        }
        path = resolved;
        let mut ancestors: Vec<_> = path.ancestors().skip(1).collect();
        let mut next = None;
        while let Some(prefix) = ancestors.pop() {
            let is_link = if let Some(attrs) = observed.get(prefix) {
                attrs.as_ref().is_some_and(Attrs::is_symlink)
            } else {
                match fs.symlink_attrs(prefix) {
                    Ok(attrs) => attrs.is_symlink(),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        break;
                    }
                    Err(error) => return Err(attributed(error)),
                }
            };
            if is_link {
                next = Some(prefix.to_path_buf());
                break;
            }
        }
        match next {
            Some(next) => link = next,
            None => return Ok(Some(path)),
        }
    }
    Err(attributed(std::io::Error::from_raw_os_error(libc::ELOOP)))
}

fn read_transfer_ancestry<'a>(
    fs: &impl Vfsi,
    paths: impl Iterator<Item = (usize, &'a Path)>,
    follow_final: bool,
    batch_size: usize,
    mut observed: HashMap<&'a Path, Option<Attrs>>,
) -> Result<HashMap<&'a Path, Option<Attrs>>> {
    let mut paths = paths;
    let options = AttrsOptions::new()
        .with_fields(Attributes::MODE | Attributes::identity())
        .with_follow_final_symlink(follow_final);
    loop {
        let inputs: Vec<_> = paths
            .by_ref()
            .filter(|&(_, path)| {
                use std::collections::hash_map::Entry;
                if let Entry::Vacant(entry) = observed.entry(path) {
                    entry.insert(None);
                    true
                } else {
                    false
                }
            })
            .take(batch_size)
            .collect();
        if inputs.is_empty() {
            break;
        }
        let targets: Vec<_> = inputs.iter().map(|&(_, path)| path).collect();
        let results = fs.vgetattrs(&targets, options).map_err(|error| {
            IndexedError::unattributed(error).with_context("transfer ancestry", targets[0])
        })?;
        if results.len() != inputs.len() {
            return Err(contract(
                "transfer: invalid ancestry metadata outcome count",
            ));
        }
        for ((root, path), result) in inputs.into_iter().zip(results) {
            match result {
                Ok(attrs) => {
                    observed.insert(path, Some(attrs));
                }
                Err(error)
                    if matches!(
                        error.cause().kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => {
                    return Err(IndexedError::at(root, error.into_cause())
                        .with_context("transfer ancestry", path));
                }
            }
        }
    }
    Ok(observed)
}

fn reject_identity_pairs<'a>(
    fs: &impl Vfsi,
    task: &Task,
    mut pairs: impl Iterator<Item = (ObservedObject<'a>, ObservedObject<'a>)>,
    batch_size: usize,
) -> Result<()> {
    loop {
        let wave: Vec<_> = pairs.by_ref().take(batch_size).collect();
        if wave.is_empty() {
            return Ok(());
        }
        let results = fs.compare_routed_objects(&wave);
        if results.len() != wave.len() {
            return Err(contract("transfer: invalid identity decision count"));
        }
        for (pair, result) in wave.into_iter().zip(results) {
            if compared_object(fs, pair, result, task.root)? {
                return Err(invalid(task.root, &task.destination));
            }
        }
    }
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
        .map(|task| crate::MkDirOp::new(&task.destination, 0o777))
        .collect();
    let outcomes = fs.vmkdir(&paths).map_err(IndexedError::unattributed)?;
    if outcomes.len() != tasks.len() {
        return Err(contract("invalid directory creation outcome count"));
    }
    for (index, result) in outcomes.into_iter().enumerate() {
        match result {
            Ok(()) => tasks[index].fresh_destination = true,
            Err(error)
                if error.state() != vfsi_core::FailureState::Unknown
                    && error.cause().kind() == std::io::ErrorKind::AlreadyExists =>
            {
                let task = &mut tasks[index];
                let attrs = fs.symlink_attrs(&task.destination).map_err(|error| {
                    mapped(IndexedError::at(0, error), std::slice::from_ref(task))
                })?;
                if !attrs.is_dir() {
                    return Err(invalid(task.root, &task.destination));
                }
                task.fresh_destination = false;
            }
            Err(error) => return Err(mapped(IndexedError::at(index, error.into_cause()), tasks)),
        }
    }
    Ok(())
}

fn run<P: AsRef<Path>>(
    fs: &impl Vfsi,
    sources: &[P],
    destination: &Path,
    options: TransferOptions,
    exact: bool,
    mut progress: Progress<'_>,
) -> Result<TransferSummary> {
    let limits = fs.limits();
    let max_entries = options
        .max_entries
        .unwrap_or(limits.max_directory_entries());
    let max_path_bytes = options
        .max_path_bytes
        .unwrap_or(limits.max_directory_path_bytes());
    let mut budget = TraversalBudget::new(max_entries, max_path_bytes);
    let mut roots = roots(
        fs,
        sources,
        destination,
        options,
        exact,
        &mut budget,
        max_path_bytes,
    )?;
    let mut summary = TransferSummary::default();
    if roots.is_empty() {
        return Ok(summary);
    }
    // The container must exist or be creatable; its parent must already exist.
    if !exact {
        let container = Task {
            source: roots[0].source.clone(),
            destination: normalize(destination, max_path_bytes)
                .map_err(IndexedError::unattributed)?,
            metadata: roots[0].metadata.clone(),
            size: 0,
            root: 0,
            depth: 0,
            fresh_destination: false,
            descriptor_copy: false,
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
    options: TransferOptions,
    budget: &mut TraversalBudget,
    summary: &mut TransferSummary,
    progress: &mut Progress<'_>,
    skipped: &mut HashSet<usize>,
) -> Result<()> {
    if options.layout != CopyLayout::Contents || roots.len() < 2 {
        return copy_independent_roots(fs, roots, options, budget, summary, progress, skipped);
    }
    // Contents roots can map to the same destination, including descendants of
    // a directory operand. Complete conflicting roots in input order. Distinct
    // regular-file basenames remain independent and retain vector I/O.
    let mut roots = roots.into_iter().peekable();
    let mut first_cohort = true;
    while let Some(root) = roots.next() {
        let regular = root.metadata.is_file();
        let mut destinations = HashSet::new();
        destinations.insert(root.destination.clone());
        let mut cohort = vec![root];
        if regular {
            while let Some(next) = roots.peek() {
                if !next.metadata.is_file() || !destinations.insert(next.destination.clone()) {
                    break;
                }
                cohort.push(roots.next().expect("peeked root"));
            }
        }
        if !first_cohort {
            // A freshly created container may now contain earlier output.
            for task in &mut cohort {
                task.fresh_destination = false;
            }
        }
        copy_independent_roots(fs, cohort, options, budget, summary, progress, skipped)?;
        if summary.stopped {
            return Ok(());
        }
        first_cohort = false;
    }
    Ok(())
}

fn copy_independent_roots(
    fs: &impl Vfsi,
    roots: Vec<Task>,
    options: TransferOptions,
    budget: &mut TraversalBudget,
    summary: &mut TransferSummary,
    progress: &mut Progress<'_>,
    skipped: &mut HashSet<usize>,
) -> Result<()> {
    let limits = fs.limits();
    let mut pending = Vec::new();
    let mut roots = roots.into_iter();
    while roots.len() != 0 {
        let batch = roots.by_ref().take(options.batch_size).collect();
        dispatch(fs, batch, &mut pending, options, summary, progress, skipped)?;
        if summary.stopped {
            return Ok(());
        }
    }
    while !pending.is_empty() && !summary.stopped {
        let wave_size = pending.len().min(options.batch_size);
        let wave: Vec<_> = pending.drain(pending.len() - wave_size..).collect();
        let paths: Vec<_> = wave.iter().map(|t| &t.source).collect();
        let options_visit = budget.remaining_options(ListDirOptions::new().with_fields(fields()));
        let mut callback_error = None;
        let completed = fs
            .listdirs_all(&paths, options_visit, |index, page| {
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
                        budget
                            .charge_paths([entry.path(), dest.as_path()])
                            .map_err(|_| limit(parent.root, entry.path()))?;
                        let depth = parent
                            .depth
                            .checked_add(1)
                            .ok_or_else(|| limit(parent.root, entry.path()))?;
                        if entry.attrs().is_dir()
                            && depth
                                > options
                                    .depth
                                    .map_or(limits.max_walk_depth().get(), |d| d.get())
                        {
                            if options.depth.is_some() {
                                summary.entries_skipped += 1;
                                skipped.insert(parent.root);
                                continue;
                            }
                            return Err(limit(parent.root, entry.path()));
                        }
                        let (source, metadata) = entry.into_parts();
                        children.push(Task {
                            source,
                            destination: dest,
                            metadata,
                            size: 0,
                            root: parent.root,
                            depth,
                            fresh_destination: parent.fresh_destination,
                            descriptor_copy: parent.descriptor_copy,
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
                match result {
                    Ok(value) => Ok(value),
                    Err(error) => {
                        let (root, cause) = error.into_parts();
                        if callback_error
                            .as_ref()
                            .is_none_or(|(prior, _)| index < *prior)
                        {
                            let mut causes = vfsi_core::internal::errors::fanout(cause, 2);
                            let cause = causes.next().unwrap();
                            callback_error = Some((
                                index,
                                match root {
                                    Some(root) => IndexedError::at(root, cause),
                                    None => IndexedError::unattributed(cause),
                                },
                            ));
                            Err(causes.next().unwrap())
                        } else {
                            Err(cause)
                        }
                    }
                }
            })
            .map_err(|e| match callback_error.take() {
                Some((input, callback)) if e.index() == Some(input) => callback,
                _ => mapped(e, &wave),
            })?;
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
    options: TransferOptions,
    summary: &mut TransferSummary,
    progress: &mut Progress<'_>,
    skipped: &mut HashSet<usize>,
) -> Result<()> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let native_copy = progress.is_none()
        && options.existing == Existing::Replace
        && !options.flags.preserve_permissions();
    for mut task in tasks {
        if task.metadata.is_dir() {
            dirs.push(task);
        } else if task.metadata.is_file() {
            task.size = task.metadata.len().ok_or_else(|| {
                IndexedError::at(
                    task.root,
                    vfsi_core::internal::errors::with_context(
                        std::io::Error::from(std::io::ErrorKind::Unsupported),
                        "transfer size",
                        &task.source,
                    ),
                )
            })?;
            files.push(task);
        } else if options.unsupported == UnsupportedEntry::Skip {
            summary.entries_skipped += 1;
            skipped.insert(task.root);
        } else {
            return Err(IndexedError::at(
                task.root,
                vfsi_core::internal::errors::with_context(
                    std::io::Error::from_raw_os_error(libc::ENOTSUP),
                    "transfer",
                    &task.source,
                ),
            ));
        }
    }
    if native_copy {
        let candidates: Vec<_> = files
            .iter()
            .filter(|task| !task.descriptor_copy)
            .map(|task| (task.source.as_path(), task.destination.as_path()))
            .collect();
        if !candidates.is_empty() {
            let supported = fs.supports_regular_path_copies(&candidates);
            if supported.len() != candidates.len() {
                return Err(contract("transfer: invalid COPY route decision count"));
            }
            for (task, supported) in files
                .iter_mut()
                .filter(|task| !task.descriptor_copy)
                .zip(supported)
            {
                task.descriptor_copy = !supported;
            }
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
            })
            .map_err(|cause| {
                IndexedError::at(task.root, cause).with_context("transfer progress", &task.source)
            })?
            .is_break()
            {
                summary.stopped = true;
                return Ok(());
            }
        }
    }
    pending.extend(dirs);
    let read_budget = fs.limits().max_read_bytes();
    let chunk = options.chunk_bytes.min(read_budget);
    let mut start = 0;
    while start < files.len() {
        let mut end = start;
        let mut bytes = 0;
        let use_native = native_copy && !files[start].descriptor_copy;
        while end < files.len() && end - start < options.batch_size {
            if (native_copy && !files[end].descriptor_copy) != use_native {
                break;
            }
            let required = chunk.min(usize::try_from(files[end].size).unwrap_or(usize::MAX));
            if !use_native && required > read_budget - bytes {
                break;
            }
            if !use_native {
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
    options: TransferOptions,
    chunk: usize,
    summary: &mut TransferSummary,
    progress: &mut Progress<'_>,
    skipped: &mut HashSet<usize>,
) -> Result<()> {
    if options.flags.preserve_permissions() {
        for task in tasks {
            if task.metadata.mode().is_none() {
                return Err(IndexedError::at(
                    task.root,
                    vfsi_core::internal::errors::with_context(
                        std::io::Error::from(std::io::ErrorKind::Unsupported),
                        "transfer permissions",
                        &task.source,
                    ),
                ));
            }
        }
    }
    if !tasks[0].descriptor_copy
        && progress.is_none()
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
        let handles = fs.open_batch(&requests).map_err(|e| mapped(e, tasks))?;
        let valid = handles.len() == tasks.len();
        let close = fs.close_files(handles).map_err(|e| mapped(e, tasks));
        if !valid {
            return Err(contract(
                "transfer: invalid native COPY reservation cardinality",
            ));
        }
        close?;
        let pairs: Vec<_> = tasks.iter().map(|t| (&t.source, &t.destination)).collect();
        fs.copies_all(&pairs, CopyOptions::default())
            .map_err(|e| mapped(e, tasks))?;
        summary.files_copied += tasks.len() as u64;
        summary.bytes_copied = None;
        return Ok(());
    }
    // Empty files need no payload storage. A zero read budget also permits
    // native COPY and rename-only moves, but cannot make descriptor progress
    // on a nonempty file. Reject before opening or replacing any file.
    if chunk == 0
        && let Some(task) = tasks.iter().find(|task| task.size != 0)
    {
        return Err(limit(task.root, &task.source));
    }
    // Reserve read buffers before any replacement. Skip mode reserves buffers
    // only for files admitted by its exclusive destination OPEN.
    let mut buffers = if options.existing == Existing::Skip {
        Vec::new()
    } else {
        allocate_transfer_buffers(tasks.iter(), chunk)?
    };
    let requests: Vec<_> = tasks
        .iter()
        .map(|t| OpenOp::for_positioned_io(&t.source, OpenFlags::READ))
        .collect();
    let sources = fs.open_batch(&requests).map_err(|e| mapped(e, tasks))?;
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
                match fs.open_batch(&[request]) {
                    Ok(mut files) => {
                        if files.len() != 1 {
                            let _ = fs.close_files(files);
                            return Err(contract("transfer: invalid destination OPEN cardinality"));
                        }
                        destinations.push(files.remove(0));
                        selected.push(index);
                    }
                    Err(e) if e.cause().kind() == std::io::ErrorKind::AlreadyExists => {
                        summary.entries_skipped += 1;
                        skipped.insert(task.root);
                    }
                    Err(e) => {
                        return Err(mapped(e, std::slice::from_ref(task)));
                    }
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
            destinations = fs.open_batch(&requests).map_err(|e| mapped(e, tasks))?;
            if destinations.len() != tasks.len() {
                return Err(contract("transfer: invalid destination OPEN cardinality"));
            }
            selected.extend(0..tasks.len());
        }
        if options.existing == Existing::Skip {
            buffers =
                allocate_transfer_buffers(selected.iter().map(|&index| &tasks[index]), chunk)?;
        }
        let mut offsets = vec![0u64; selected.len()];
        let mut done = vec![false; selected.len()];
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
                .read_batch(reads, ReadOptions::new())
                .map_err(|e| active_error(e, &active, &selected, tasks))?;
            if read.len() != active.len() {
                return Err(contract("transfer: invalid READ cardinality"));
            }
            for (result, i) in read.iter().zip(&active) {
                let remaining = tasks[selected[*i]].size.saturating_sub(offsets[*i]);
                if result.offset() != offsets[*i]
                    || result.data().is_some()
                    || result.bytes_read()
                        > buffers[*i]
                            .len()
                            .min(usize::try_from(remaining).unwrap_or(usize::MAX))
                {
                    return Err(contract("transfer: misassociated READ result"));
                }
                if result.bytes_read() == 0 && !result.eof() && remaining != 0 {
                    return Err(contract("transfer: READ made no progress"));
                }
            }
            let writes: Vec<_> = read
                .iter()
                .zip(&active)
                .map(|(r, i)| {
                    WriteOp::at(
                        &destinations[*i],
                        offsets[*i],
                        &buffers[*i][..r.bytes_read()],
                    )
                })
                .collect();
            let written = fs
                .write_batch(&writes, WriteOptions::new().with_write_all(true))
                .map_err(|e| active_error(e, &active, &selected, tasks))?;
            if written.len() != read.len() {
                return Err(contract("transfer: invalid WRITE cardinality"));
            }
            for ((written, read), i) in written.iter().zip(&read).zip(&active) {
                if written.offset != offsets[*i] || written.bytes_written != read.bytes_read() {
                    return Err(contract("transfer: invalid complete WRITE progress"));
                }
                offsets[*i] = offsets[*i]
                    .checked_add(read.bytes_read() as u64)
                    .ok_or_else(|| limit(tasks[selected[*i]].root, &tasks[selected[*i]].source))?;
                if let Some(bytes) = &mut summary.bytes_copied {
                    *bytes = bytes.checked_add(read.bytes_read() as u64).ok_or_else(|| {
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
                    })
                    .map_err(|cause| {
                        IndexedError::at(task.root, cause)
                            .with_context("transfer progress", &task.source)
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
            fs.setattrs_all(&updates)
                .map_err(|e| mapped(e, &completed))?;
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
            e.map_index(|_| task.root)
                .with_context("transfer", &task.destination)
        } else {
            e
        }
    });
    operation?;
    close_sources?;
    close_destinations
}
fn allocate_transfer_buffers<'a>(
    tasks: impl Iterator<Item = &'a Task>,
    chunk: usize,
) -> Result<Vec<Vec<u8>>> {
    tasks
        .map(|task| {
            let length = chunk.min(usize::try_from(task.size).unwrap_or(usize::MAX));
            vfsi_core::internal::zeroed_read_buffer(length).map_err(|cause| {
                IndexedError::at(task.root, cause)
                    .with_context("transfer read buffer", &task.source)
            })
        })
        .collect()
}

fn active_error(
    error: IndexedError,
    active: &[usize],
    selected: &[usize],
    tasks: &[Task],
) -> IndexedError {
    if let Some(task) = error
        .index()
        .and_then(|i| active.get(i))
        .and_then(|i| selected.get(*i))
        .and_then(|i| tasks.get(*i))
    {
        error
            .map_index(|_| task.root)
            .with_context("transfer", &task.source)
    } else {
        error
    }
}
fn same_file(
    fs: &impl Vfsi,
    source_path: &Path,
    source: &Attrs,
    destination_path: &Path,
    destination: &Attrs,
    root: usize,
) -> Result<bool> {
    if source_path == destination_path {
        // Comparing two snapshots of the same source detects replacement.
        return Ok(source.fs_obj_id().is_some() && source.fs_obj_id() == destination.fs_obj_id());
    }
    let pair = ((source_path, source), (destination_path, destination));
    let mut results = fs.compare_routed_objects(&[pair]);
    if results.len() != 1 {
        return Err(contract("transfer: invalid identity decision count"));
    }
    compared_object(fs, pair, results.remove(0), root)
}

fn compared_object(
    fs: &impl Vfsi,
    ((left_path, left), (right_path, right)): (ObservedObject<'_>, ObservedObject<'_>),
    result: std::io::Result<Option<bool>>,
    root: usize,
) -> Result<bool> {
    let routed = result.map_err(|cause| {
        IndexedError::at(root, cause).with_context("transfer identity", right_path)
    })?;
    Ok(routed.unwrap_or_else(|| {
        left.fs_obj_id().is_some()
            && left.fs_obj_id() == right.fs_obj_id()
            && fs.same_object_namespace(left_path, right_path)
    }))
}
fn prepare_replace(fs: &impl Vfsi, tasks: &[Task]) -> Result<()> {
    let paths: Vec<_> = tasks
        .iter()
        .filter(|task| !task.fresh_destination)
        .map(|task| &task.destination)
        .collect();
    if paths.is_empty() {
        return Ok(());
    }
    let outcomes = fs
        .vgetattrs(
            &paths,
            AttrsOptions::new()
                .with_fields(Attributes::stat())
                .with_follow_final_symlink(false),
        )
        .map_err(IndexedError::unattributed)?;
    if outcomes.len() != paths.len() {
        return Err(contract(
            "transfer: invalid replacement metadata outcome count",
        ));
    }
    let observed: Vec<_> = tasks
        .iter()
        .filter(|task| !task.fresh_destination)
        .zip(outcomes)
        .collect();
    let pairs: Vec<_> = observed
        .iter()
        .filter_map(|(task, outcome)| {
            let attrs = outcome.as_ref().ok().filter(|attrs| attrs.is_file())?;
            Some((
                (task.source.as_path(), &task.metadata),
                (task.destination.as_path(), attrs),
            ))
        })
        .collect();
    let identities = fs.compare_routed_objects(&pairs);
    if identities.len() != pairs.len() {
        return Err(contract("transfer: invalid identity decision count"));
    }
    let mut comparisons = pairs.into_iter().zip(identities);
    let mut existing = Vec::new();
    let mut failure = None;
    for (index, (task, outcome)) in observed.iter().enumerate() {
        match outcome {
            Ok(meta) => {
                if !meta.is_file() {
                    return Err(invalid(task.root, &task.destination));
                }
                let (pair, result) = comparisons.next().expect("identity for each existing file");
                if compared_object(fs, pair, result, task.root)? {
                    return Err(invalid(task.root, &task.destination));
                }
                existing.push(*task);
            }
            Err(error) if error.cause().kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                failure = Some(index);
                break;
            }
        }
    }
    if let Some(index) = failure {
        let (task, outcome) = observed.into_iter().nth(index).unwrap();
        return Err(mapped(
            IndexedError::at(0, outcome.unwrap_err().into_cause()),
            std::slice::from_ref(task),
        ));
    }
    if !existing.is_empty() {
        let paths: Vec<_> = existing.iter().map(|t| &t.destination).collect();
        fs.removes_all(
            &paths,
            crate::RemoveMode::Entry,
            crate::RemoveOptions::new(),
        )
        .map_err(|e| mapped(e, &existing))?;
    }
    Ok(())
}

/// Move roots within the client's namespace. Rename is attempted only for
/// Container+Replace without progress or a depth override, using an atomic
/// absent-destination check. Cross-device, existing-destination and unsupported
/// rename errors permit copy/delete fallback after reconciling partial progress.
/// Other modes copy first; source deletion occurs only after complete copying
/// and successful explicit CLOSE, with no skipped entries. Cancellation/errors
/// retain sources that have not already been renamed or removed.
/// Stable/quiescent source trees are required: this is not a snapshot or a
/// transactional move. An error deleting a source can leave both copies.
pub fn move_items<P: AsRef<Path>>(
    fs: &impl Vfsi,
    sources: &[P],
    destination: impl AsRef<Path>,
    options: TransferOptions,
) -> Result<TransferSummary> {
    move_run(fs, sources, destination.as_ref(), options, None)
}
pub fn move_items_with_progress<P: AsRef<Path>>(
    fs: &impl Vfsi,
    sources: &[P],
    destination: impl AsRef<Path>,
    options: TransferOptions,
    mut progress: impl FnMut(TransferProgress<'_>) -> std::io::Result<ControlFlow<()>>,
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
    options: TransferOptions,
    mut progress: Progress<'_>,
) -> Result<TransferSummary> {
    let limits = fs.limits();
    let max_entries = options
        .max_entries
        .unwrap_or(limits.max_directory_entries());
    let max_path_bytes = options
        .max_path_bytes
        .unwrap_or(limits.max_directory_path_bytes());
    let mut budget = TraversalBudget::new(max_entries, max_path_bytes);
    // Validate all roots/overlaps before the first mutation.
    let mut tasks = roots(
        fs,
        sources,
        destination,
        options,
        false,
        &mut budget,
        max_path_bytes,
    )?;
    let mut summary = TransferSummary::default();
    if let Some(task) = tasks.first() {
        let container = Task {
            source: task.source.clone(),
            destination: normalize(destination, max_path_bytes)
                .map_err(IndexedError::unattributed)?,
            metadata: task.metadata.clone(),
            size: 0,
            root: 0,
            depth: 0,
            fresh_destination: false,
            descriptor_copy: false,
        };
        let mut containers = [container];
        ensure_dirs(fs, &mut containers)?;
        for task in &mut tasks {
            task.fresh_destination = containers[0].fresh_destination;
        }
    }
    // Contents operands can share destinations. Only the first root can rely
    // on a newly created container: earlier roots may create later targets.
    // Container roots were preflighted as independent and can be vectorized.
    let cohort = if options.layout == CopyLayout::Contents {
        for task in tasks.iter_mut().skip(1) {
            task.fresh_destination = false;
        }
        1
    } else {
        options.batch_size
    };
    let can_rename = progress.is_none()
        && options.layout == CopyLayout::Container
        && options.existing == Existing::Replace
        && options.depth.is_none();
    for batch in tasks.chunks(cohort) {
        let mut copies = Vec::new();
        let mut renames = Vec::new();
        for task in batch {
            if !task.metadata.is_file() && !task.metadata.is_dir() {
                summary.entries_skipped += 1;
                continue;
            }
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
            let outcomes = match fs.vrename(&pairs, crate::RenameMode::NoReplace) {
                Ok(outcomes) => outcomes,
                Err(cause) => vfsi_core::internal::errors::fanout(cause, renames.len())
                    .map(|cause| Err(vfsi_core::OpError::not_attempted(cause)))
                    .collect(),
            };
            if outcomes.len() != renames.len() {
                return Err(contract("rename outcome count differs from requests"));
            }
            for (index, (task, result)) in renames.iter().zip(outcomes).enumerate() {
                match result {
                    Ok(()) => summary.roots_renamed += 1,
                    Err(error) => {
                        if error.progress().is_some() {
                            return Err(mapped(
                                IndexedError::at(
                                    index,
                                    vfsi_core::internal::errors::invalid_reply(
                                        "rename cannot report byte progress",
                                    ),
                                ),
                                &renames,
                            ));
                        }
                        let fallback = error.state() != vfsi_core::FailureState::Unknown
                            && !vfsi_core::transport_kind(error.cause()).is_some()
                            && matches!(
                                error.cause().kind(),
                                std::io::ErrorKind::CrossesDevices
                                    | std::io::ErrorKind::AlreadyExists
                                    | std::io::ErrorKind::Unsupported
                            );
                        if !fallback {
                            return Err(mapped(
                                IndexedError::at(index, error.into_cause()),
                                &renames,
                            ));
                        }
                        prepare_rename_fallback(fs, task, error.into_cause(), &mut copies)?;
                    }
                }
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
            fs.removes_all(&paths, crate::RemoveMode::Tree, crate::RemoveOptions::new())
                .map_err(|error| mapped(error, &complete))?;
            summary.roots_removed += complete.len() as u64;
        }
    }
    Ok(summary)
}

fn prepare_rename_fallback(
    fs: &impl Vfsi,
    task: &Task,
    failure: Error,
    copies: &mut Vec<Task>,
) -> Result<()> {
    let tasks = std::slice::from_ref(task);
    let source = match fs.symlink_attrs(&task.source) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(mapped(IndexedError::at(0, failure), tasks));
        }
        Err(error) => return Err(mapped(IndexedError::at(0, error), tasks)),
    };
    if task.metadata.fs_obj_id().is_some()
        && !same_file(
            fs,
            &task.source,
            &task.metadata,
            &task.source,
            &source,
            task.root,
        )?
    {
        return Err(invalid(task.root, &task.source));
    }
    match fs.symlink_attrs(&task.destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(mapped(IndexedError::at(0, error), tasks)),
        Ok(target) => {
            if same_file(
                fs,
                &task.source,
                &task.metadata,
                &task.destination,
                &target,
                task.root,
            )? {
                return Err(mapped(IndexedError::at(0, failure), tasks));
            }
        }
    }
    let mut fallback = task.clone();
    // Cross-device rename proves pathname COPY may use incompatible routes.
    // Carry the descriptor strategy to descendants before replacing anything.
    fallback.descriptor_copy = failure.kind() == std::io::ErrorKind::CrossesDevices;
    copies.push(fallback);
    Ok(())
}

#[cfg(all(test, feature = "posix", unix))]
mod object_identity_tests {
    use super::*;
    #[test]
    fn same_file_requires_complete_filesystem_identity() {
        let root = tempfile::tempdir().unwrap();
        let fs = crate::Posix::new(root.path()).unwrap();
        let attrs = |fs_id, returned| {
            vfsi_core::metadata_from_attrs(vfsi_core::VfAttrs {
                fs_id,
                fileid: 7,
                returned,
                ..Default::default()
            })
        };
        let a = attrs(1, Attributes::identity());
        let alias = attrs(1, Attributes::identity());
        let other_fs = attrs(2, Attributes::identity());
        let incomplete = attrs(1, Attributes::FILEID);
        assert!(same_file(&fs, Path::new("/a"), &a, Path::new("/alias"), &alias, 0,).unwrap());
        assert!(!same_file(&fs, Path::new("/a"), &a, Path::new("/other"), &other_fs, 0,).unwrap());
        assert!(
            !same_file(
                &fs,
                Path::new("/a"),
                &a,
                Path::new("/unknown"),
                &incomplete,
                0,
            )
            .unwrap()
        );
    }
}
