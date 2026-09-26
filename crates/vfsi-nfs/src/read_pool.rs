//! Bounded, ordered read-ahead across independent NFS sessions.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use vfsi_core::{VfError, VfResult};
use vfsi_sync::{FsClient, FsFile};

use super::{NfsClientBuilder, NfsVecFs};

/// Controls concurrency and memory use for [`NfsReadPool`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NfsReadPoolOptions {
    worker_count: usize,
    chunk_size: usize,
    max_in_flight: usize,
    max_buffered_bytes: usize,
}

impl NfsReadPoolOptions {
    pub const fn new() -> Self {
        Self {
            worker_count: 4,
            chunk_size: 1024 * 1024,
            max_in_flight: 8,
            max_buffered_bytes: 16 * 1024 * 1024,
        }
    }

    pub const fn worker_count(mut self, count: usize) -> Self {
        self.worker_count = count;
        self
    }

    pub const fn chunk_size(mut self, bytes: usize) -> Self {
        self.chunk_size = bytes;
        self
    }

    /// Maximum scheduled ranges not yet delivered to the callback.
    pub const fn max_in_flight(mut self, count: usize) -> Self {
        self.max_in_flight = count;
        self
    }

    /// Hard budget for completed and in-progress range buffers.
    pub const fn max_buffered_bytes(mut self, bytes: usize) -> Self {
        self.max_buffered_bytes = bytes;
        self
    }

    pub const fn configured_worker_count(self) -> usize {
        self.worker_count
    }

    pub const fn configured_chunk_size(self) -> usize {
        self.chunk_size
    }

    pub const fn configured_max_in_flight(self) -> usize {
        self.max_in_flight
    }

    pub const fn configured_max_buffered_bytes(self) -> usize {
        self.max_buffered_bytes
    }

    fn effective(self) -> VfResult<(usize, usize)> {
        if self.worker_count == 0
            || self.chunk_size == 0
            || self.max_in_flight == 0
            || self.max_buffered_bytes < self.chunk_size
        {
            return Err(VfError::client(0, vfsi_core::ERR_INVAL));
        }
        let in_flight = self
            .max_in_flight
            .min(self.max_buffered_bytes / self.chunk_size);
        Ok((self.worker_count.min(in_flight), in_flight))
    }
}

impl Default for NfsReadPoolOptions {
    fn default() -> Self {
        Self::new()
    }
}

enum WorkerCommand {
    Start {
        stream_id: u64,
        path: PathBuf,
    },
    Read {
        stream_id: u64,
        index: u64,
        offset: u64,
        length: usize,
    },
    Finish {
        stream_id: u64,
    },
    Shutdown,
}

enum WorkerEvent {
    Ready {
        worker: usize,
        result: VfResult<()>,
    },
    Started {
        stream_id: u64,
        worker: usize,
        result: VfResult<Option<u64>>,
    },
    Chunk {
        stream_id: u64,
        worker: usize,
        index: u64,
        offset: u64,
        result: VfResult<Vec<u8>>,
    },
    Finished {
        stream_id: u64,
        worker: usize,
        result: VfResult<()>,
    },
    Stopped {
        worker: usize,
    },
}

/// Reusable bounded pool that pipelines positional reads over separate NFS sessions.
///
/// The pool owns one persistent connection per worker. A stream is not a
/// snapshot: concurrent modifications can be observed differently by its
/// independent range requests. Use it for stable large files or add
/// application-level versioning.
pub struct NfsReadPool {
    commands: Vec<SyncSender<WorkerCommand>>,
    events: Receiver<WorkerEvent>,
    threads: Vec<JoinHandle<()>>,
    options: NfsReadPoolOptions,
    effective_workers: usize,
    effective_in_flight: usize,
    worker_alive: Vec<bool>,
    next_stream_id: u64,
}

impl std::fmt::Debug for NfsReadPool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NfsReadPool")
            .field("options", &self.options)
            .field("workers", &self.effective_workers)
            .field("max_in_flight", &self.effective_in_flight)
            .finish_non_exhaustive()
    }
}

impl NfsReadPool {
    pub(super) fn connect(
        builder: NfsClientBuilder,
        options: NfsReadPoolOptions,
    ) -> VfResult<Self> {
        let (worker_count, in_flight) = options.effective()?;
        let (event_tx, events) = mpsc::channel();
        let pool_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut commands = Vec::with_capacity(worker_count);
        let mut threads = Vec::with_capacity(worker_count);
        for worker in 0..worker_count {
            let (command_tx, command_rx) = mpsc::sync_channel(in_flight);
            commands.push(command_tx);
            let worker_builder = unique_worker_builder(builder.clone(), pool_id, worker);
            let worker_events = event_tx.clone();
            let run_events = worker_events.clone();
            let thread = spawn_worker_thread(worker, worker_events, move || {
                worker_main(worker, worker_builder, command_rx, run_events)
            })
            .map_err(|error| {
                VfError::transport(None, format!("failed to spawn read worker: {error}"))
            })?;
            threads.push(thread);
        }
        drop(event_tx);

        let mut pool = Self {
            commands,
            events,
            threads,
            options,
            effective_workers: worker_count,
            effective_in_flight: in_flight,
            worker_alive: vec![true; worker_count],
            next_stream_id: 1,
        };
        let mut first_error = None;
        let mut ready_workers = vec![false; worker_count];
        for _ in 0..worker_count {
            match pool.events.recv() {
                Ok(WorkerEvent::Ready { worker, result })
                    if worker < worker_count && !ready_workers[worker] =>
                {
                    ready_workers[worker] = true;
                    if let Err(error) = result {
                        pool.worker_alive[worker] = false;
                        first_error.get_or_insert(error);
                    }
                }
                Ok(WorkerEvent::Stopped { worker }) if worker < worker_count => {
                    pool.worker_alive[worker] = false;
                    first_error.get_or_insert_with(|| {
                        VfError::transport(
                            None,
                            format!("read worker {worker} stopped during startup"),
                        )
                    });
                }
                Ok(_) => {
                    first_error.get_or_insert_with(|| {
                        VfError::transport(None, "read worker sent an invalid startup response")
                    });
                }
                Err(_) => {
                    first_error.get_or_insert_with(|| {
                        VfError::transport(None, "read worker startup channel closed")
                    });
                    break;
                }
            }
        }
        if let Some(error) = first_error {
            drop(pool);
            return Err(error);
        }
        Ok(pool)
    }

    /// Stream a file in order while independent sessions fetch ranges ahead.
    ///
    /// Return `Ok(false)` to cancel successfully. Callback errors propagate.
    /// Cancellation waits for already-scheduled reads to finish before
    /// returning, so outstanding memory remains bounded by the configured
    /// in-flight and byte limits. The callback must not reenter this pool.
    pub fn read_stream(
        &mut self,
        path: impl AsRef<Path>,
        mut callback: impl FnMut(u64, &[u8]) -> VfResult<bool>,
    ) -> VfResult<()> {
        let path = path.as_ref().to_path_buf();
        let stream_id = self.next_stream_id;
        self.next_stream_id = self.next_stream_id.wrapping_add(1).max(1);

        let mut active_workers = Vec::new();
        let mut primary_error = None;
        for (worker, command) in self.commands.iter().enumerate() {
            if command
                .send(WorkerCommand::Start {
                    stream_id,
                    path: path.clone(),
                })
                .is_ok()
            {
                active_workers.push(worker);
            } else {
                self.worker_alive[worker] = false;
                primary_error.get_or_insert_with(|| {
                    VfError::transport(None, "read worker stopped before stream start")
                });
            }
        }

        let mut started = 0;
        let mut started_workers = vec![false; self.effective_workers];
        let mut file_size = None;
        while started < active_workers.len() {
            match self.events.recv() {
                Ok(WorkerEvent::Started {
                    stream_id: result_stream,
                    worker,
                    result,
                }) if result_stream == stream_id
                    && worker < self.effective_workers
                    && active_workers.contains(&worker)
                    && !started_workers[worker] =>
                {
                    started += 1;
                    started_workers[worker] = true;
                    match result {
                        Ok(Some(size)) if worker == 0 => file_size = Some(size),
                        Ok(None) if worker != 0 => {}
                        Ok(_) => {
                            primary_error.get_or_insert_with(|| {
                                VfError::transport(None, "invalid read worker file-size response")
                            });
                        }
                        Err(error) => {
                            primary_error
                                .get_or_insert(error.with_context("read_stream_pipelined", &path));
                        }
                    }
                }
                Ok(WorkerEvent::Stopped { worker })
                    if worker < self.effective_workers
                        && active_workers.contains(&worker)
                        && !started_workers[worker] =>
                {
                    started += 1;
                    started_workers[worker] = true;
                    self.worker_alive[worker] = false;
                    primary_error.get_or_insert_with(|| {
                        VfError::transport(
                            None,
                            format!("read worker {worker} stopped during stream start"),
                        )
                    });
                }
                Ok(_) => {
                    primary_error.get_or_insert_with(|| {
                        VfError::transport(None, "read worker sent an unexpected stream response")
                    });
                }
                Err(_) => {
                    primary_error.get_or_insert_with(|| {
                        VfError::transport(None, "read worker channel closed during stream start")
                    });
                    break;
                }
            }
        }

        if primary_error.is_none() && file_size.is_none() {
            primary_error = Some(VfError::transport(None, "read worker omitted file size"));
        }
        let size = file_size.unwrap_or(0);
        let chunk_size = self.options.chunk_size;
        let total_ranges = if size == 0 {
            0
        } else {
            size.div_ceil(chunk_size as u64)
        };
        let mut next_job = 0u64;
        let mut next_delivery = 0u64;
        let mut jobs = BTreeMap::<u64, usize>::new();
        let mut pending = BTreeMap::new();
        let mut cancelled = false;
        let mut callback_error = None;
        let mut callback_panic = None;

        while primary_error.is_none() && !cancelled && next_delivery < total_ranges {
            while next_job < total_ranges
                && next_job.saturating_sub(next_delivery) < self.effective_in_flight as u64
            {
                let offset = next_job
                    .checked_mul(chunk_size as u64)
                    .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?;
                let length = (size - offset).min(chunk_size as u64) as usize;
                let worker = (next_job as usize) % self.effective_workers;
                if self.commands[worker]
                    .send(WorkerCommand::Read {
                        stream_id,
                        index: next_job,
                        offset,
                        length,
                    })
                    .is_err()
                {
                    primary_error = Some(VfError::transport(
                        None,
                        format!("read worker {worker} stopped before range {next_job}"),
                    ));
                    self.worker_alive[worker] = false;
                    break;
                }
                jobs.insert(next_job, worker);
                next_job += 1;
            }
            if primary_error.is_some() {
                break;
            }

            match self.events.recv() {
                Ok(WorkerEvent::Chunk {
                    stream_id: result_stream,
                    worker,
                    index,
                    offset,
                    result,
                }) if result_stream == stream_id => {
                    if jobs.remove(&index) != Some(worker) {
                        primary_error.get_or_insert_with(|| {
                            VfError::transport(None, "read worker returned an unassigned range")
                        });
                        continue;
                    }
                    match result {
                        Ok(data) => {
                            let expected_offset = index.saturating_mul(chunk_size as u64);
                            if expected_offset >= size {
                                primary_error = Some(VfError::transport(
                                    None,
                                    format!("worker returned out-of-range index {index}"),
                                ));
                                continue;
                            }
                            let expected_len =
                                (size - expected_offset).min(chunk_size as u64) as usize;
                            if offset != expected_offset || data.len() != expected_len {
                                primary_error = Some(VfError::transport(
                                    None,
                                    format!("worker returned malformed range {index}"),
                                ));
                                continue;
                            }
                            if pending.insert(index, data).is_some() {
                                primary_error = Some(VfError::transport(
                                    None,
                                    format!("worker returned range {index} more than once"),
                                ));
                                continue;
                            }
                            while let Some(data) = pending.remove(&next_delivery) {
                                let start = next_delivery * chunk_size as u64;
                                match catch_unwind(AssertUnwindSafe(|| callback(start, &data))) {
                                    Ok(Ok(true)) => next_delivery += 1,
                                    Ok(Ok(false)) => {
                                        next_delivery += 1;
                                        cancelled = true;
                                        break;
                                    }
                                    Ok(Err(error)) => {
                                        callback_error = Some(error);
                                        next_delivery += 1;
                                        cancelled = true;
                                        break;
                                    }
                                    Err(payload) => {
                                        callback_panic = Some(payload);
                                        next_delivery += 1;
                                        cancelled = true;
                                        break;
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            let index = usize::try_from(index).unwrap_or(usize::MAX);
                            primary_error = Some(
                                error
                                    .with_index(index)
                                    .with_context("read_stream_pipelined", &path),
                            );
                        }
                    }
                }
                Ok(WorkerEvent::Stopped { worker }) if worker < self.effective_workers => {
                    self.worker_alive[worker] = false;
                    let stopped_jobs: Vec<_> = jobs
                        .iter()
                        .filter_map(|(&index, &assigned)| (assigned == worker).then_some(index))
                        .collect();
                    for index in stopped_jobs {
                        jobs.remove(&index);
                    }
                    primary_error.get_or_insert_with(|| {
                        VfError::transport(
                            None,
                            format!("read worker {worker} stopped during range reads"),
                        )
                    });
                }
                Ok(_) => {
                    primary_error.get_or_insert_with(|| {
                        VfError::transport(None, "read worker sent an unexpected range response")
                    });
                }
                Err(_) => {
                    primary_error.get_or_insert_with(|| {
                        VfError::transport(None, "read worker channel closed during range reads")
                    });
                    break;
                }
            }
        }

        // Stop scheduling after cancellation/failure, but drain every request
        // already sent before closing worker descriptors.
        while !jobs.is_empty() {
            match self.events.recv() {
                Ok(WorkerEvent::Chunk {
                    stream_id: result_stream,
                    worker,
                    index,
                    result,
                    ..
                }) if result_stream == stream_id => {
                    if jobs.remove(&index) != Some(worker) {
                        primary_error.get_or_insert_with(|| {
                            VfError::transport(
                                None,
                                "read worker returned an unassigned range while draining",
                            )
                        });
                        continue;
                    }
                    if primary_error.is_none()
                        && let Err(error) = result
                    {
                        primary_error = Some(error.with_context("read_stream_pipelined", &path));
                    }
                }
                Ok(WorkerEvent::Stopped { worker }) if worker < self.effective_workers => {
                    self.worker_alive[worker] = false;
                    let stopped_jobs: Vec<_> = jobs
                        .iter()
                        .filter_map(|(&index, &assigned)| (assigned == worker).then_some(index))
                        .collect();
                    for index in stopped_jobs {
                        jobs.remove(&index);
                    }
                    primary_error.get_or_insert_with(|| {
                        VfError::transport(
                            None,
                            format!("read worker {worker} stopped while draining"),
                        )
                    });
                }
                Ok(_) => {
                    primary_error.get_or_insert_with(|| {
                        VfError::transport(None, "read worker sent an unexpected drain response")
                    });
                }
                Err(_) => {
                    primary_error.get_or_insert_with(|| {
                        VfError::transport(None, "read worker channel closed while draining")
                    });
                    break;
                }
            }
        }

        let close_error = self.finish_stream(stream_id, &active_workers);
        if let Some(payload) = callback_panic {
            std::panic::resume_unwind(payload);
        }
        if let Some(error) = primary_error {
            return Err(error);
        }
        if let Some(error) = callback_error {
            return Err(error);
        }
        close_error
    }

    fn finish_stream(&mut self, stream_id: u64, active_workers: &[usize]) -> VfResult<()> {
        let mut error = None;
        let mut awaiting = Vec::new();
        for &worker in active_workers {
            if !self.worker_alive[worker] {
                continue;
            }
            if self.commands[worker]
                .send(WorkerCommand::Finish { stream_id })
                .is_err()
            {
                self.worker_alive[worker] = false;
                error.get_or_insert_with(|| {
                    VfError::transport(None, "read worker stopped before stream cleanup")
                });
            } else {
                awaiting.push(worker);
            }
        }
        let mut finished = 0;
        let mut finished_workers = vec![false; self.effective_workers];
        while finished < awaiting.len() {
            match self.events.recv() {
                Ok(WorkerEvent::Finished {
                    stream_id: result_stream,
                    worker,
                    result,
                }) if result_stream == stream_id
                    && worker < self.effective_workers
                    && awaiting.contains(&worker)
                    && !finished_workers[worker] =>
                {
                    finished += 1;
                    finished_workers[worker] = true;
                    if let Err(close_error) = result {
                        error.get_or_insert(close_error);
                    }
                }
                Ok(WorkerEvent::Stopped { worker })
                    if worker < self.effective_workers
                        && awaiting.contains(&worker)
                        && !finished_workers[worker] =>
                {
                    finished += 1;
                    finished_workers[worker] = true;
                    self.worker_alive[worker] = false;
                    error.get_or_insert_with(|| {
                        VfError::transport(
                            None,
                            format!("read worker {worker} stopped during cleanup"),
                        )
                    });
                }
                Ok(_) => {
                    error.get_or_insert_with(|| {
                        VfError::transport(None, "read worker sent an unexpected cleanup response")
                    });
                }
                Err(_) => {
                    error.get_or_insert_with(|| {
                        VfError::transport(None, "read worker channel closed during cleanup")
                    });
                    self.worker_alive.fill(false);
                    break;
                }
            }
        }
        error.map_or(Ok(()), Err)
    }
}

impl Drop for NfsReadPool {
    fn drop(&mut self) {
        for command in &self.commands {
            let _ = command.send(WorkerCommand::Shutdown);
        }
        self.commands.clear();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn unique_worker_builder(
    mut builder: NfsClientBuilder,
    pool_id: u128,
    worker: usize,
) -> NfsClientBuilder {
    if let Some(owner) = &mut builder.options.client_owner {
        let suffix = format!("-read-pool-{}-{pool_id:x}-{worker}", std::process::id());
        let keep = 1024usize.saturating_sub(suffix.len());
        owner.truncate(keep);
        owner.extend_from_slice(suffix.as_bytes());
    }
    builder
}

fn spawn_worker_thread(
    worker: usize,
    events: mpsc::Sender<WorkerEvent>,
    run: impl FnOnce() + Send + 'static,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("vnfs-read-pool-{worker}"))
        .spawn(move || {
            let panic_events = events.clone();
            if catch_unwind(AssertUnwindSafe(run)).is_err() {
                let _ = panic_events.send(WorkerEvent::Stopped { worker });
            }
        })
}

fn worker_main(
    worker: usize,
    builder: NfsClientBuilder,
    commands: Receiver<WorkerCommand>,
    events: mpsc::Sender<WorkerEvent>,
) {
    let client = match builder.connect() {
        Ok(backend) => FsClient::new(backend),
        Err(error) => {
            let _ = events.send(WorkerEvent::Ready {
                worker,
                result: Err(error),
            });
            return;
        }
    };
    if events
        .send(WorkerEvent::Ready {
            worker,
            result: Ok(()),
        })
        .is_err()
    {
        return;
    }

    let mut active: Option<(u64, FsFile<NfsVecFs>)> = None;
    while let Ok(command) = commands.recv() {
        match command {
            WorkerCommand::Start { stream_id, path } => {
                let result = (|| {
                    if active.is_some() {
                        return Err(VfError::client(0, libc::EBUSY as u32));
                    }
                    let size = if worker == 0 {
                        Some(client.metadata(&path)?.len())
                    } else {
                        None
                    };
                    let file = client.open(&path)?;
                    active = Some((stream_id, file));
                    Ok(size)
                })();
                let _ = events.send(WorkerEvent::Started {
                    stream_id,
                    worker,
                    result,
                });
            }
            WorkerCommand::Read {
                stream_id,
                index,
                offset,
                length,
            } => {
                let result = match active.as_ref() {
                    Some((active_id, file)) if *active_id == stream_id => {
                        read_range(file, offset, length).map_err(|error| {
                            error.with_index(usize::try_from(index).unwrap_or(usize::MAX))
                        })
                    }
                    _ => Err(VfError::client(
                        usize::try_from(index).unwrap_or(usize::MAX),
                        vfsi_core::ERR_EBADF,
                    )),
                };
                let _ = events.send(WorkerEvent::Chunk {
                    stream_id,
                    worker,
                    index,
                    offset,
                    result,
                });
            }
            WorkerCommand::Finish { stream_id } => {
                let result = match active.take() {
                    Some((active_id, file)) if active_id == stream_id => file.close(),
                    Some(active_file) => {
                        active = Some(active_file);
                        Err(VfError::client(0, vfsi_core::ERR_INVAL))
                    }
                    None => Ok(()),
                };
                let _ = events.send(WorkerEvent::Finished {
                    stream_id,
                    worker,
                    result,
                });
            }
            WorkerCommand::Shutdown => {
                if let Some((_, file)) = active.take() {
                    let _ = file.close();
                }
                return;
            }
        }
    }
    if let Some((_, file)) = active.take() {
        let _ = file.close();
    }
}

fn read_range(file: &FsFile<NfsVecFs>, offset: u64, length: usize) -> VfResult<Vec<u8>> {
    let mut data = vec![0; length];
    let mut filled = 0;
    while filled < length {
        let current_offset = offset
            .checked_add(filled as u64)
            .ok_or_else(|| VfError::client(0, libc::EOVERFLOW as u32))?;
        let count = file.read_at(&mut data[filled..], current_offset)?;
        if count == 0 {
            return Err(
                VfError::client(0, libc::EIO as u32).with_context("read_pipeline", file.path())
            );
        }
        filled += count;
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::{NfsReadPoolOptions, WorkerEvent, spawn_worker_thread};
    use std::sync::mpsc;

    #[test]
    fn options_enforce_minimums_and_cap_concurrency_by_memory_budget() {
        assert!(
            NfsReadPoolOptions::new()
                .worker_count(0)
                .effective()
                .is_err()
        );
        assert!(NfsReadPoolOptions::new().chunk_size(0).effective().is_err());
        assert!(
            NfsReadPoolOptions::new()
                .max_buffered_bytes(1024)
                .effective()
                .is_err()
        );

        // A 3-block budget prevents an 8-request window from allocating more
        // than 3 chunks, regardless of the worker count.
        assert_eq!(
            NfsReadPoolOptions::new()
                .worker_count(8)
                .chunk_size(4096)
                .max_in_flight(8)
                .max_buffered_bytes(12 * 1024)
                .effective()
                .unwrap(),
            (3, 3)
        );
    }

    #[test]
    fn panicking_worker_emits_a_terminal_event() {
        let (events, received) = mpsc::channel();
        let thread = spawn_worker_thread(2, events, || panic!("injected worker panic"))
            .expect("spawn test worker");
        assert!(matches!(
            received.recv().expect("worker termination event"),
            WorkerEvent::Stopped { worker: 2 }
        ));
        thread.join().expect("panic is contained by worker wrapper");
    }
}
