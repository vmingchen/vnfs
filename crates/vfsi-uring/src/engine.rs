use crate::Options;
use io_uring::{IoUring, opcode, types};
use std::{
    fs::File,
    io,
    os::fd::AsRawFd,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use vfsi_local::io::{DescriptorIo, OwnedRead, Read, Write};

#[derive(Default)]
struct Counters {
    syscall_reads: AtomicU64,
    syscall_writes: AtomicU64,
    cache_probes: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    waves: AtomicU64,
    submissions: AtomicU64,
    completions: AtomicU64,
    peak_bytes: AtomicU64,
    enters: AtomicU64,
    scratch_growths: AtomicU64,
    copied_read_bytes: AtomicU64,
}

/// Cumulative execution counters; a snapshot may span an ongoing operation.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Ordinary positional reads selected by a warm-cohort NOWAIT hit.
    pub syscall_reads: u64,
    /// Nonempty logical writes sent to the synchronous syscall executor.
    pub syscall_writes: u64,
    /// Synchronous RWF_NOWAIT read attempts, excluding ring fallback work.
    pub cache_probes: u64,
    /// Requests completely satisfied by synchronous RWF_NOWAIT probes.
    pub cache_hits: u64,
    /// Requests routed to the ring by cache-read selection.
    pub cache_misses: u64,
    pub waves: u64,
    pub submissions: u64,
    pub completions: u64,
    pub peak_bytes: u64,
    /// Calls to io_uring_enter, including interrupted attempts.
    pub enters: u64,
    /// Growths of the executor's reusable owned scratch arena.
    pub scratch_growths: u64,
    /// Bytes copied from scratch into borrowed read destinations.
    pub copied_read_bytes: u64,
}

#[derive(Clone, Default)]
pub struct Telemetry(Arc<Counters>);

impl Telemetry {
    pub fn snapshot(&self) -> Stats {
        let counters = &self.0;
        Stats {
            syscall_reads: counters.syscall_reads.load(Ordering::Relaxed),
            syscall_writes: counters.syscall_writes.load(Ordering::Relaxed),
            cache_probes: counters.cache_probes.load(Ordering::Relaxed),
            cache_hits: counters.cache_hits.load(Ordering::Relaxed),
            cache_misses: counters.cache_misses.load(Ordering::Relaxed),
            waves: counters.waves.load(Ordering::Relaxed),
            submissions: counters.submissions.load(Ordering::Relaxed),
            completions: counters.completions.load(Ordering::Relaxed),
            peak_bytes: counters.peak_bytes.load(Ordering::Relaxed),
            enters: counters.enters.load(Ordering::Relaxed),
            scratch_growths: counters.scratch_growths.load(Ordering::Relaxed),
            copied_read_bytes: counters.copied_read_bytes.load(Ordering::Relaxed),
        }
    }
}

pub(crate) struct Engine {
    ring: Option<IoUring>,
    options: Options,
    telemetry: Telemetry,
    scratch: Vec<u8>,
    nowait_supported: bool,
    progress: Vec<usize>,
    done: Vec<bool>,
    errors: Vec<Option<i32>>,
    groups: Vec<std::ops::Range<usize>>,
    entries: Vec<io_uring::squeue::Entry>,
    results: Vec<Option<i32>>,
    pending: Vec<Pending>,
    #[cfg(test)]
    cache_read_limit: Option<usize>,
    #[cfg(test)]
    cache_read_error: Option<(usize, i32)>,
    #[cfg(test)]
    cache_read_calls: std::cell::Cell<usize>,
    #[cfg(test)]
    short_completion: Option<usize>,
    #[cfg(test)]
    fail_after_submit: bool,
}

enum Buffer<'a> {
    Read(&'a mut [u8]),
    OwnedRead(Vec<u8>),
    Write(&'a [u8]),
    Sync(bool),
}
struct Job<'a> {
    file: &'a Arc<File>,
    offset: u64,
    buffer: Buffer<'a>,
}
#[derive(Clone, Copy)]
enum Kind {
    Read,
    Write,
    Sync(bool),
}
struct Pending {
    // Own descriptors and final owned-read buffers addressed by SQEs. Borrowed
    // buffers use the owned scratch arena; none are referenced by the kernel.
    file: Arc<File>,
    bytes: std::ops::Range<usize>,
    offset: u64,
    kind: Kind,
    job: usize,
    owned: Option<Vec<u8>>,
    owned_start: usize,
    result: i32,
}

impl Engine {
    pub(crate) fn new(options: Options) -> io::Result<(Self, Telemetry)> {
        if options.queue_depth.get() > 4096 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // Cooperative task work avoids completion IPIs while the synchronous
        // caller is already waiting. Unlike SINGLE_ISSUER it permits clients
        // to be used from different threads. Older kernels keep ordinary rings.
        let ring = IoUring::builder()
            .setup_coop_taskrun()
            .build(options.queue_depth.get())
            .or_else(|error| {
                if error.raw_os_error() == Some(libc::EINVAL) {
                    IoUring::new(options.queue_depth.get())
                } else {
                    Err(error)
                }
            })?;
        let mut probe = io_uring::Probe::new();
        ring.submitter().register_probe(&mut probe)?;
        if [opcode::Read::CODE, opcode::Write::CODE, opcode::Fsync::CODE]
            .iter()
            .any(|code| !probe.is_supported(*code))
        {
            return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
        }
        let telemetry = Telemetry::default();
        Ok((
            Self {
                ring: Some(ring),
                options,
                telemetry: telemetry.clone(),
                scratch: Vec::new(),
                nowait_supported: true,
                progress: Vec::new(),
                done: Vec::new(),
                errors: Vec::new(),
                groups: Vec::new(),
                entries: Vec::new(),
                results: Vec::new(),
                pending: Vec::new(),
                #[cfg(test)]
                cache_read_limit: None,
                #[cfg(test)]
                cache_read_error: None,
                #[cfg(test)]
                cache_read_calls: std::cell::Cell::new(0),
                #[cfg(test)]
                short_completion: None,
                #[cfg(test)]
                fail_after_submit: false,
            },
            telemetry,
        ))
    }

    fn cached_read(&self, op: &mut Read<'_>) -> io::Result<usize> {
        #[cfg(test)]
        {
            let calls = self.cache_read_calls.get();
            self.cache_read_calls.set(calls + 1);
            if let Some((after, errno)) = self.cache_read_error
                && calls >= after
            {
                return Err(io::Error::from_raw_os_error(errno));
            }
        }
        let length = op.buffer.len();
        #[cfg(test)]
        if let Some(limit) = self.cache_read_limit {
            // A deterministic cache-hit/partial-hit seam, transferring actual
            // bytes even on test filesystems that cannot implement NOWAIT.
            use std::os::unix::fs::FileExt;
            return op
                .file
                .read_at(&mut op.buffer[..length.min(limit)], op.offset);
        }
        let vector = libc::iovec {
            iov_base: op.buffer.as_mut_ptr().cast(),
            iov_len: length,
        };
        // SAFETY: this synchronous syscall borrows one initialized, writable
        // buffer and a live descriptor only until it returns. RWF_NOWAIT never
        // schedules asynchronous access to caller storage on EAGAIN.
        let count = unsafe {
            libc::preadv2(
                op.file.as_raw_fd(),
                &vector,
                1,
                op.offset as libc::off_t,
                libc::RWF_NOWAIT,
            )
        };
        if count < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(count as usize)
        }
    }

    fn wave(&mut self, mut pending: Vec<Pending>) -> io::Result<Vec<Pending>> {
        let Some(ring) = self.ring.as_mut() else {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        };
        // Take ownership before publishing pointers. On success the arena is
        // reused; on a catastrophic failure it is retained with the descriptors.
        let mut scratch = std::mem::take(&mut self.scratch);
        let scratch_len = scratch.len();
        // Derive all SQE pointers from one base pointer. Repeated mutable slice
        // borrows could invalidate earlier pointers into this shared arena.
        let scratch_ptr = scratch.as_mut_ptr();
        // Adjacent positional writes already occupy adjacent arena storage.
        // One transfer avoids scheduling multiple workers against one inode.
        // Keep the logical requests for precise short-write progress and errors;
        // never combine reads, gaps, reordered ranges, or different descriptors.
        let groups = &mut self.groups;
        groups.clear();
        for (index, op) in pending.iter().enumerate() {
            if let Some(group) = groups.last_mut() {
                let first = &pending[group.start];
                let last = &pending[group.end - 1];
                if matches!((first.kind, op.kind), (Kind::Write, Kind::Write))
                    && Arc::ptr_eq(&first.file, &op.file)
                    && last.bytes.end == op.bytes.start
                    && first
                        .offset
                        .checked_add((op.bytes.start - first.bytes.start) as u64)
                        == Some(op.offset)
                    && op.bytes.end - first.bytes.start <= u32::MAX as usize
                {
                    group.end = index + 1;
                    continue;
                }
            }
            groups.push(index..index + 1);
        }
        // Complete all allocation and fallible preparation before publishing
        // any pointers into the SQ. Vector storage is not moved/reallocated
        // until every CQE has arrived.
        let entries = &mut self.entries;
        entries.clear();
        entries.extend(groups.iter().enumerate().map(|(index, group)| {
            let end = pending[group.end - 1].bytes.end;
            let op = &mut pending[group.start];
            let fd = types::Fd(op.file.as_raw_fd());
            let length = (end - op.bytes.start) as u32;
            // SAFETY: final read buffers and scratch ranges were bounded
            // before submission. Neither storage is resized/accessed until
            // all CQEs drain. Vec header moves do not move allocations.
            let bytes = if let Some(buffer) = &mut op.owned {
                debug_assert!(op.owned_start + length as usize <= buffer.len());
                unsafe { buffer.as_mut_ptr().add(op.owned_start) }
            } else {
                debug_assert!(end <= scratch_len);
                unsafe { scratch_ptr.add(op.bytes.start) }
            };
            // Inject an actual short transfer, not a fabricated CQE after
            // the full payload has already reached the file.
            #[cfg(test)]
            let length = self.short_completion.map_or(length, |limit| {
                length.min(u32::try_from(limit).unwrap_or(u32::MAX))
            });
            let entry = match op.kind {
                Kind::Read => opcode::Read::new(fd, bytes, length)
                    .offset(op.offset)
                    .build(),
                Kind::Write => opcode::Write::new(fd, bytes.cast_const(), length)
                    .offset(op.offset)
                    .build(),
                Kind::Sync(data_only) => opcode::Fsync::new(fd)
                    .flags(if data_only {
                        types::FsyncFlags::DATASYNC
                    } else {
                        types::FsyncFlags::empty()
                    })
                    .build(),
            };
            entry.user_data(index as u64)
        }));
        let results = &mut self.results;
        results.clear();
        results.resize(entries.len(), None);
        let bytes: usize = pending.iter().map(|op| op.bytes.len()).sum();
        let counters = &self.telemetry.0;
        counters
            .peak_bytes
            .fetch_max(bytes as u64, Ordering::Relaxed);
        counters.waves.fetch_add(1, Ordering::Relaxed);
        let result = (|| {
            {
                let mut queue = ring.submission();
                for entry in entries.iter() {
                    // SAFETY: owned buffers/files are retained until completion;
                    // this empty ring has capacity >= the bounded wave length.
                    unsafe { queue.push(entry) }
                        .map_err(|_| io::Error::from_raw_os_error(libc::EIO))?;
                }
            }
            let mut remaining = entries.len();
            while remaining != 0 {
                counters.enters.fetch_add(1, Ordering::Relaxed);
                // The synchronous API cannot return before the whole wave is
                // drained. Waiting for one CQE just adds wakeups and syscalls.
                match ring.submit_and_wait(remaining) {
                    Ok(submitted) => {
                        counters
                            .submissions
                            .fetch_add(submitted as u64, Ordering::Relaxed);
                        #[cfg(test)]
                        if self.fail_after_submit {
                            return Err(io::Error::from_raw_os_error(libc::EIO));
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
                for completion in ring.completion() {
                    let index = completion.user_data() as usize;
                    if index >= results.len() || results[index].is_some() {
                        return Err(io::Error::from_raw_os_error(libc::EIO));
                    }
                    let group = &groups[index];
                    let length =
                        pending[group.end - 1].bytes.end - pending[group.start].bytes.start;
                    let result = completion.result();
                    results[index] = Some(
                        if (result >= 0 && result as usize > length)
                            || (result == 0 && matches!(pending[group.start].kind, Kind::Write))
                        {
                            -libc::EIO
                        } else {
                            result
                        },
                    );
                    remaining -= 1;
                    counters.completions.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            // Ring teardown alone is not proof that kernel references are gone.
            // Poison the executor, never replay writes, and deliberately retain
            // in-flight resources on catastrophic submission failure. An owned
            // result may be larger than its wave slice; retain its entire final
            // allocation, bounded by the caller's read/output budget.
            // Normal negative CQEs are drained and do NOT leak resources.
            self.ring.take();
            std::mem::forget(pending);
            std::mem::forget(scratch);
            return Err(error);
        }
        self.scratch = scratch;
        for (group, result) in groups.iter().zip(results.iter()) {
            let result = result.unwrap();
            let mut remaining = result.max(0) as usize;
            for op in &mut pending[group.clone()] {
                let count = remaining.min(op.bytes.len());
                remaining -= count;
                op.result = if result < 0 { result } else { count as i32 };
            }
        }
        Ok(pending)
    }

    fn execute(&mut self, jobs: &mut [Job<'_>]) -> Vec<io::Result<usize>> {
        if self.ring.is_none() {
            return jobs
                .iter()
                .map(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
                .collect();
        }
        let mut progress = std::mem::take(&mut self.progress);
        let mut done = std::mem::take(&mut self.done);
        let mut errors = std::mem::take(&mut self.errors);
        let mut pending = std::mem::take(&mut self.pending);
        let mut copied_read_bytes = 0;
        progress.clear();
        progress.resize(jobs.len(), 0);
        done.clear();
        done.resize(jobs.len(), false);
        errors.clear();
        errors.resize(jobs.len(), None);
        while done.iter().any(|finished| !finished) {
            pending.clear();
            let mut budget = self.options.max_batch_bytes.get();
            for (index, job) in jobs.iter_mut().enumerate() {
                if done[index] {
                    continue;
                }
                if pending.len() == self.options.queue_depth.get() as usize {
                    break;
                }
                let (kind, length) = match &job.buffer {
                    Buffer::Read(buffer) => (Kind::Read, buffer.len()),
                    Buffer::OwnedRead(buffer) => (Kind::Read, buffer.len()),
                    Buffer::Write(buffer) => (Kind::Write, buffer.len()),
                    Buffer::Sync(data_only) => (Kind::Sync(*data_only), 0),
                };
                if length == progress[index] && !matches!(kind, Kind::Sync(_)) {
                    done[index] = true;
                    continue;
                }
                if budget == 0 && length != 0 {
                    break;
                }
                let count = (length - progress[index])
                    .min(budget)
                    .min(u32::MAX as usize);
                let Some(offset) = job.offset.checked_add(progress[index] as u64) else {
                    errors[index] = Some(libc::EOVERFLOW);
                    done[index] = true;
                    continue;
                };
                let file = Arc::clone(job.file);
                let start = self.options.max_batch_bytes.get() - budget;
                let bytes = start..start + count;
                budget -= count;
                pending.push(Pending {
                    file,
                    bytes,
                    offset,
                    kind,
                    job: index,
                    owned: match &mut job.buffer {
                        Buffer::OwnedRead(buffer) => Some(std::mem::take(buffer)),
                        _ => None,
                    },
                    owned_start: progress[index],
                    result: 0,
                });
            }
            if pending.is_empty() {
                continue;
            }
            let bytes = pending
                .iter()
                .filter(|op| op.owned.is_none())
                .map(|op| op.bytes.end)
                .max()
                .unwrap_or(0);
            if bytes > self.scratch.len() {
                // reserve_exact avoids geometric growth beyond the configured
                // byte budget. Already initialized read storage needs no reset:
                // only bytes acknowledged by a CQE are copied to the caller.
                self.scratch.reserve_exact(bytes - self.scratch.len());
                self.scratch.resize(bytes, 0);
                self.telemetry
                    .0
                    .scratch_growths
                    .fetch_add(1, Ordering::Relaxed);
            }
            for op in &pending {
                if let Buffer::Write(data) = &jobs[op.job].buffer {
                    let start = progress[op.job];
                    self.scratch[op.bytes.clone()]
                        .copy_from_slice(&data[start..start + op.bytes.len()]);
                }
            }
            match self.wave(std::mem::take(&mut pending)) {
                Ok(mut completed) => {
                    for op in &mut completed {
                        let result = op.result;
                        let index = op.job;
                        if let Buffer::OwnedRead(buffer) = &mut jobs[index].buffer {
                            *buffer = op.owned.take().unwrap();
                        }
                        if result < 0 {
                            errors[index] = Some(-result);
                            done[index] = true;
                            continue;
                        }
                        let count = result as usize;
                        if count > op.bytes.len() {
                            errors[index] = Some(libc::EIO);
                            done[index] = true;
                            continue;
                        }
                        if let Buffer::Read(buffer) = &mut jobs[index].buffer {
                            buffer[progress[index]..progress[index] + count].copy_from_slice(
                                &self.scratch[op.bytes.start..op.bytes.start + count],
                            );
                            copied_read_bytes += count as u64;
                        }
                        progress[index] += count;
                        // A short read ends this operation; no fixed-offset gap
                        // concatenation. Short writes retry the contiguous suffix.
                        done[index] = match op.kind {
                            Kind::Read => {
                                count < op.bytes.len()
                                    || match &jobs[index].buffer {
                                        Buffer::Read(buffer) => progress[index] == buffer.len(),
                                        Buffer::OwnedRead(buffer) => {
                                            progress[index] == buffer.len()
                                        }
                                        _ => unreachable!(),
                                    }
                            }
                            // A merged short write may stop before this member.
                            // Actual zero-progress WRITE CQEs were rejected in
                            // wave(); these zero counts are unattempted suffixes.
                            Kind::Write => false,
                            Kind::Sync(_) => true,
                        };
                    }
                    completed.clear();
                    pending = completed;
                }
                Err(error) => {
                    let errno = error.raw_os_error().unwrap_or(libc::EIO);
                    for index in 0..jobs.len() {
                        if !done[index] {
                            errors[index] = Some(errno);
                            done[index] = true;
                        }
                    }
                }
            }
            // wave() has drained every published SQE. A known failure must
            // not dispatch untouched jobs or retry another job's suffix.
            // Keep their acknowledged progress; the indexed error remains
            // attached to the request that actually failed.
            if errors.iter().any(Option::is_some) {
                break;
            }
        }
        let results = progress
            .iter()
            .zip(&errors)
            .map(|(&count, &error)| {
                error.map_or(Ok(count), |errno| Err(io::Error::from_raw_os_error(errno)))
            })
            .collect();
        // Do not retain metadata for an arbitrarily large caller cohort for
        // the client's lifetime. Ordinary queue-sized batches reuse it; larger
        // batches release these arrays after their results have been assembled.
        if jobs.len() <= self.options.queue_depth.get() as usize {
            self.progress = progress;
            self.done = done;
            self.errors = errors;
        }
        self.pending = pending;
        self.telemetry
            .0
            .copied_read_bytes
            .fetch_add(copied_read_bytes, Ordering::Relaxed);
        results
    }
}

impl DescriptorIo for Engine {
    fn owned_read_window(&self) -> Option<usize> {
        (!self.options.flags.cached_reads()).then_some(self.options.max_batch_bytes.get())
    }
    fn read_owned(&mut self, mut operations: Vec<OwnedRead>) -> Vec<io::Result<Vec<u8>>> {
        // Direct final destinations win for bounded small cohorts. Large
        // working sets currently copy substantially faster through the reused
        // arena on the measured ARM64 kernel; keep that proven path rather
        // than trading away large-file throughput to remove a copy everywhere.
        let fits_window = operations
            .iter()
            .try_fold(0usize, |bytes, op| bytes.checked_add(op.buffer.len()))
            .is_some_and(|bytes| bytes <= self.options.max_batch_bytes.get());
        if self.options.flags.cached_reads() || !fits_window {
            return vfsi_local::io::read_owned_via_borrowed(self, operations);
        }
        let mut jobs: Vec<_> = operations
            .iter_mut()
            .map(|op| Job {
                file: &op.file,
                offset: op.offset,
                buffer: Buffer::OwnedRead(std::mem::take(&mut op.buffer)),
            })
            .collect();
        let results = self.execute(&mut jobs);
        jobs.into_iter()
            .zip(results)
            .map(|(job, result)| {
                let count = result?;
                let Buffer::OwnedRead(mut buffer) = job.buffer else {
                    unreachable!()
                };
                buffer.truncate(count);
                Ok(buffer)
            })
            .collect()
    }
    fn ordered(&self) -> bool {
        self.options.flags.syscall_writes()
    }
    fn read(&mut self, operations: &mut [Read<'_>]) -> Vec<io::Result<usize>> {
        if self.options.flags.cached_reads()
            && self.nowait_supported
            && self.ring.is_some()
            && operations
                .first()
                .is_some_and(|op| op.buffer.len() <= 64 * 1024)
        {
            let mut results = Vec::with_capacity(operations.len());
            let mut fallback = Vec::new();
            let mut indices = Vec::new();
            let mut failed = false;
            let mut probe = true;
            let mut warm_left = 0;
            let (mut probes, mut hits, mut misses, mut direct) = (0, 0, 0, 0);
            for (index, op) in operations.iter_mut().enumerate() {
                if failed {
                    results.push(Ok(0));
                    continue;
                }
                if op.buffer.is_empty() {
                    results.push(Ok(0));
                    continue;
                }
                // Require two cache hits before selecting ordinary reads;
                // one hot file alone is weak evidence about its siblings.
                let probing = warm_left == 0 || warm_left == 15;
                let result = if probe && op.buffer.len() <= 64 * 1024 {
                    if probing {
                        probes += 1;
                        self.cached_read(op)
                    } else {
                        use std::os::unix::fs::FileExt;
                        direct += 1;
                        op.file.read_at(op.buffer, op.offset)
                    }
                } else {
                    Err(io::Error::from_raw_os_error(libc::EAGAIN))
                };
                let prefix = match result {
                    Ok(count) if count == op.buffer.len() => {
                        if probing {
                            hits += 1;
                        }
                        if warm_left == 0 {
                            warm_left = 15;
                        } else {
                            warm_left -= 1;
                        }
                        results.push(Ok(count));
                        continue;
                    }
                    Ok(count) => {
                        // Confirm zero/partial NOWAIT results through the ring:
                        // neither is evidence that the file has reached EOF.
                        probe = false;
                        count
                    }
                    Err(error) if error.raw_os_error() == Some(libc::EAGAIN) => {
                        // One miss is enough to select ring batching for the
                        // rest of this call. Do not issue one cold probe per file.
                        probe = false;
                        0
                    }
                    Err(error)
                        if matches!(
                            error.raw_os_error(),
                            Some(libc::EOPNOTSUPP | libc::ENOSYS)
                        ) =>
                    {
                        self.nowait_supported = false;
                        probe = false;
                        0
                    }
                    Err(error) => {
                        results.push(Err(error));
                        failed = true;
                        continue;
                    }
                };
                let Some(offset) = op.offset.checked_add(prefix as u64) else {
                    results.push(Err(io::Error::from_raw_os_error(libc::EOVERFLOW)));
                    failed = true;
                    continue;
                };
                misses += 1;
                results.push(Ok(prefix));
                indices.push(index);
                fallback.push(Read {
                    file: op.file,
                    offset,
                    buffer: &mut op.buffer[prefix..],
                });
            }
            self.telemetry
                .0
                .cache_probes
                .fetch_add(probes, Ordering::Relaxed);
            self.telemetry
                .0
                .syscall_reads
                .fetch_add(direct, Ordering::Relaxed);
            self.telemetry
                .0
                .cache_hits
                .fetch_add(hits, Ordering::Relaxed);
            self.telemetry
                .0
                .cache_misses
                .fetch_add(misses, Ordering::Relaxed);
            if !failed && !fallback.is_empty() {
                // Partial cache hits continue at their actual offsets. Their
                // uncached suffixes share bounded ring waves with other misses.
                let mut jobs: Vec<_> = fallback
                    .iter_mut()
                    .map(|op| Job {
                        file: op.file,
                        offset: op.offset,
                        buffer: Buffer::Read(op.buffer),
                    })
                    .collect();
                for (index, result) in indices.into_iter().zip(self.execute(&mut jobs)) {
                    results[index] = result.map(|count| count + results[index].as_ref().unwrap());
                }
            }
            return results;
        }
        let mut jobs: Vec<_> = operations
            .iter_mut()
            .map(|op| Job {
                file: op.file,
                offset: op.offset,
                buffer: Buffer::Read(op.buffer),
            })
            .collect();
        self.execute(&mut jobs)
    }
    fn write(&mut self, operations: &[Write<'_>]) -> Vec<io::Result<usize>> {
        if self.options.flags.syscall_writes() && self.ring.is_some() {
            let results = vfsi_local::io::write_ordered(operations);
            self.telemetry.0.syscall_writes.fetch_add(
                operations
                    .iter()
                    .zip(&results)
                    .filter(|(op, result)| !op.data.is_empty() && !matches!(result, Ok(0)))
                    .count() as u64,
                Ordering::Relaxed,
            );
            return results;
        }
        let mut jobs: Vec<_> = operations
            .iter()
            .map(|op| Job {
                file: op.file,
                offset: op.offset,
                buffer: Buffer::Write(op.data),
            })
            .collect();
        self.execute(&mut jobs)
    }
    fn sync(&mut self, files: &[&Arc<File>], data_only: bool) -> Vec<io::Result<usize>> {
        let mut jobs: Vec<_> = files
            .iter()
            .map(|file| Job {
                file,
                offset: 0,
                buffer: Buffer::Sync(data_only),
            })
            .collect();
        self.execute(&mut jobs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfsi_core::{ReadOp, VfOffset, WriteOp};
    use vfsi_sync::backend::{HandleBackend, VectorBackend};

    #[test]
    fn owned_reads_fill_final_allocations_without_copying_and_reuse_bookkeeping() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"0123456789").unwrap();
        let file = Arc::new(File::open(path).unwrap());
        for short in [None, Some(2), Some(0)] {
            let (mut engine, telemetry) = Engine::new(
                Options::default()
                    .queue_depth(std::num::NonZeroU32::new(3).unwrap())
                    .max_batch_bytes(std::num::NonZeroUsize::new(16).unwrap()),
            )
            .unwrap();
            engine.short_completion = short;
            let mut capacities = None;
            for _ in 0..3 {
                let buffer = vec![99; 8];
                let pointer = buffer.as_ptr();
                let results = engine.read_owned(vec![
                    OwnedRead {
                        file: Arc::clone(&file),
                        offset: 1,
                        buffer,
                    },
                    OwnedRead {
                        file: Arc::clone(&file),
                        offset: 8,
                        buffer: vec![99; 4],
                    },
                    OwnedRead {
                        file: Arc::clone(&file),
                        offset: 0,
                        buffer: Vec::new(),
                    },
                ]);
                let expected = match short {
                    None => b"12345678".as_slice(),
                    Some(2) => b"12",
                    Some(0) => b"",
                    _ => unreachable!(),
                };
                assert_eq!(results[0].as_ref().unwrap(), expected);
                assert_eq!(
                    results[0].as_ref().unwrap().as_ptr(),
                    pointer,
                    "the final allocation must be returned unchanged"
                );
                // The byte budget partitions the second file; a short CQE
                // stops at its actual contiguous prefix, never skips bytes.
                let second = results[1].as_ref().unwrap();
                assert!(b"89".starts_with(second));
                if short.is_none() {
                    assert_eq!(second, b"89");
                }
                if short == Some(0) {
                    assert!(second.is_empty());
                }
                assert!(results[2].as_ref().unwrap().is_empty());
                let current = (
                    engine.progress.capacity(),
                    engine.done.capacity(),
                    engine.errors.capacity(),
                    engine.groups.capacity(),
                    engine.entries.capacity(),
                    engine.results.capacity(),
                );
                if let Some(previous) = capacities {
                    assert_eq!(current, previous);
                }
                capacities = Some(current);
                assert!(engine.progress.capacity() >= 3);
            }
            let stats = telemetry.snapshot();
            assert_eq!((stats.copied_read_bytes, stats.scratch_growths), (0, 0));
            assert_eq!(stats.submissions, stats.completions);
            assert!(stats.peak_bytes <= 16);
            assert!(engine.scratch.is_empty());
            let mut borrowed = [99; 2];
            engine.short_completion = None;
            assert_eq!(
                *engine.read(&mut [Read {
                    file: &file,
                    offset: 2,
                    buffer: &mut borrowed
                }])[0]
                    .as_ref()
                    .unwrap(),
                2
            );
            assert_eq!(borrowed, *b"23");
            assert_eq!(telemetry.snapshot().copied_read_bytes, 2);
        }
    }

    #[test]
    fn owned_cohorts_larger_than_the_window_keep_bounded_scratch_then_return_to_direct_reads() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"abcdefgh").unwrap();
        let file = Arc::new(File::open(path).unwrap());
        let (mut engine, telemetry) = Engine::new(
            Options::default().max_batch_bytes(std::num::NonZeroUsize::new(3).unwrap()),
        )
        .unwrap();
        let results = engine.read_owned(vec![OwnedRead {
            file: Arc::clone(&file),
            offset: 0,
            buffer: vec![99; 8],
        }]);
        assert_eq!(results[0].as_ref().unwrap(), b"abcdefgh");
        let stats = telemetry.snapshot();
        assert_eq!(
            (stats.waves, stats.copied_read_bytes, stats.peak_bytes),
            (3, 8, 3)
        );
        let results = engine.read_owned(vec![OwnedRead {
            file,
            offset: 2,
            buffer: vec![99; 3],
        }]);
        assert_eq!(results[0].as_ref().unwrap(), b"cde");
        assert_eq!(telemetry.snapshot().copied_read_bytes, 8);
        assert_eq!(telemetry.snapshot().scratch_growths, 1);
    }

    #[test]
    fn owned_read_errors_drain_a_wave_stop_unsent_work_and_allow_reuse() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"abcd").unwrap();
        let file = Arc::new(File::open(&path).unwrap());
        let writeonly = Arc::new(File::options().write(true).open(path).unwrap());
        let (mut engine, telemetry) =
            Engine::new(Options::default().queue_depth(std::num::NonZeroU32::new(2).unwrap()))
                .unwrap();
        let results = engine.read_owned(vec![
            OwnedRead {
                file: Arc::clone(&file),
                offset: 0,
                buffer: vec![99; 4],
            },
            OwnedRead {
                file: writeonly,
                offset: 0,
                buffer: vec![99; 4],
            },
            OwnedRead {
                file: Arc::clone(&file),
                offset: 0,
                buffer: vec![99; 4],
            },
        ]);
        assert_eq!(results[0].as_ref().unwrap(), b"abcd");
        assert_eq!(
            results[1].as_ref().unwrap_err().raw_os_error(),
            Some(libc::EBADF)
        );
        assert!(results[2].as_ref().unwrap().is_empty());
        assert_eq!(
            (
                telemetry.snapshot().submissions,
                telemetry.snapshot().completions
            ),
            (2, 2)
        );
        assert!(
            engine.progress.is_empty(),
            "oversized cohort metadata must not be retained"
        );
        let results = engine.read_owned(vec![OwnedRead {
            file,
            offset: 1,
            buffer: vec![0; 3],
        }]);
        assert_eq!(results[0].as_ref().unwrap(), b"bcd");
        assert_eq!(telemetry.snapshot().copied_read_bytes, 0);
    }

    #[test]
    fn owned_reads_retain_inflight_allocations_and_descriptors_after_publication_failure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"abcd").unwrap();
        let file = Arc::new(File::open(path).unwrap());
        let weak = Arc::downgrade(&file);
        let (mut engine, telemetry) = Engine::new(Options::default()).unwrap();
        engine.fail_after_submit = true;
        let results = engine.read_owned(vec![OwnedRead {
            file,
            offset: 0,
            buffer: vec![99; 4],
        }]);
        assert_eq!(
            results[0].as_ref().unwrap_err().raw_os_error(),
            Some(libc::EIO)
        );
        assert!(engine.ring.is_none());
        assert!(
            weak.upgrade().is_some(),
            "the in-flight descriptor is retained with its owned destination"
        );
        let before = telemetry.snapshot().submissions;
        assert!(
            engine.read_owned(vec![OwnedRead {
                file: weak.upgrade().unwrap(),
                offset: 0,
                buffer: vec![0; 4]
            }])[0]
                .is_err()
        );
        assert_eq!(telemetry.snapshot().submissions, before);
        assert_eq!(telemetry.snapshot().copied_read_bytes, 0);
    }

    #[test]
    fn syscall_writes_stop_after_errors_and_cannot_bypass_poisoning() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"old").unwrap();
        let write = Arc::new(File::options().write(true).open(&path).unwrap());
        let read = Arc::new(File::open(&path).unwrap());
        let (mut engine, telemetry) = Engine::new(Options::default().syscall_writes(true)).unwrap();
        assert!(engine.ordered());
        let operations = [
            Write {
                file: &write,
                offset: 0,
                data: b"A",
            },
            Write {
                file: &read,
                offset: 1,
                data: b"B",
            },
            Write {
                file: &write,
                offset: 2,
                data: b"C",
            },
        ];
        let results = engine.write(&operations);
        assert_eq!(*results[0].as_ref().unwrap(), 1);
        assert_eq!(
            results[1].as_ref().unwrap_err().raw_os_error(),
            Some(libc::EBADF)
        );
        assert_eq!(*results[2].as_ref().unwrap(), 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"Ald");
        let stats = telemetry.snapshot();
        assert_eq!(
            (stats.syscall_writes, stats.waves, stats.submissions),
            (2, 0, 0)
        );
        engine.ring.take();
        assert!(engine.write(&operations).iter().all(Result::is_err));
        assert_eq!(std::fs::read(path).unwrap(), b"Ald");
        assert_eq!(telemetry.snapshot().syscall_writes, 2);
    }

    #[test]
    fn cached_reads_preserve_partial_prefixes_and_batch_misses_without_repeated_probes() {
        for (error, limit, hits, probes, misses, submissions) in [
            (None, usize::MAX, 2, 2, 0, 0),
            (None, 3, 0, 1, 2, 2),
            (Some(libc::EAGAIN), usize::MAX, 0, 1, 2, 2),
            (Some(libc::EOPNOTSUPP), usize::MAX, 0, 1, 2, 2),
            (Some(libc::ENOSYS), usize::MAX, 0, 1, 2, 2),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("file");
            std::fs::write(&path, b"abcdefgh").unwrap();
            let file = Arc::new(File::open(path).unwrap());
            let (mut engine, telemetry) =
                Engine::new(Options::default().cached_reads(true)).unwrap();
            engine.cache_read_limit = Some(limit);
            engine.cache_read_error = error.map(|errno| (0, errno));
            let mut first = [99; 4];
            let mut second = [99; 4];
            for round in 1..=2 {
                let results = engine.read(&mut [
                    Read {
                        file: &file,
                        offset: 0,
                        buffer: &mut first,
                    },
                    Read {
                        file: &file,
                        offset: 4,
                        buffer: &mut second,
                    },
                ]);
                assert!(results.iter().all(|result| *result.as_ref().unwrap() == 4));
                assert_eq!((first, second), (*b"abcd", *b"efgh"));
                let stats = telemetry.snapshot();
                let unsupported = matches!(error, Some(libc::EOPNOTSUPP | libc::ENOSYS));
                let probes_rounds = if unsupported { 1 } else { round };
                assert_eq!(stats.cache_probes, probes * probes_rounds);
                assert_eq!(stats.cache_hits, hits * round);
                assert_eq!(stats.syscall_reads, 0);
                assert_eq!(stats.cache_misses, misses * probes_rounds);
                assert_eq!(stats.submissions, submissions * round);
                assert_eq!(stats.completions, stats.submissions);
                assert_eq!(engine.nowait_supported, !unsupported);
            }
        }
    }

    #[test]
    fn warm_cohorts_reprobe_and_keep_original_indices_when_the_next_cohort_is_cold() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        let expected: Vec<u8> = (0..128).collect();
        std::fs::write(&path, &expected).unwrap();
        let file = Arc::new(File::open(path).unwrap());
        let (mut engine, telemetry) = Engine::new(Options::default().cached_reads(true)).unwrap();
        engine.cache_read_limit = Some(usize::MAX);
        engine.cache_read_error = Some((2, libc::EAGAIN));
        let mut buffers = [[99; 4]; 32];
        let mut operations: Vec<_> = buffers
            .iter_mut()
            .enumerate()
            .map(|(i, buffer)| Read {
                file: &file,
                offset: (i * 4) as u64,
                buffer,
            })
            .collect();
        let results = engine.read(&mut operations);
        assert_eq!(results.len(), 32);
        assert!(results.iter().all(|result| *result.as_ref().unwrap() == 4));
        assert_eq!(buffers.concat(), expected);
        let stats = telemetry.snapshot();
        assert_eq!(
            (stats.cache_probes, stats.cache_hits, stats.syscall_reads),
            (3, 2, 14)
        );
        assert_eq!(
            (
                stats.cache_misses,
                stats.waves,
                stats.submissions,
                stats.completions
            ),
            (16, 1, 16, 16)
        );
    }

    #[test]
    fn cached_reads_keep_eof_tail_and_stop_after_a_real_error() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"abc").unwrap();
        let file = Arc::new(File::open(&path).unwrap());
        let writeonly = Arc::new(File::options().write(true).open(path).unwrap());
        let (mut engine, telemetry) = Engine::new(Options::default().cached_reads(true)).unwrap();
        engine.cache_read_limit = Some(usize::MAX);
        let mut bytes = [99; 5];
        let results = engine.read(&mut [Read {
            file: &file,
            offset: 0,
            buffer: &mut bytes,
        }]);
        assert_eq!(*results[0].as_ref().unwrap(), 3);
        assert_eq!(bytes, [b'a', b'b', b'c', 99, 99]);
        assert_eq!(
            telemetry.snapshot().submissions,
            1,
            "partial cache hits must probe EOF"
        );
        let before = telemetry.snapshot().cache_probes;
        assert_eq!(
            *engine.read(&mut [Read {
                file: &writeonly,
                offset: 0,
                buffer: &mut [],
            }])[0]
                .as_ref()
                .unwrap(),
            0
        );
        assert_eq!(
            telemetry.snapshot().cache_probes,
            before,
            "empty reads do not issue syscalls"
        );
        engine.cache_read_limit = Some(0);
        let results = engine.read(&mut [Read {
            file: &file,
            offset: 0,
            buffer: &mut bytes[..3],
        }]);
        assert_eq!(
            *results[0].as_ref().unwrap(),
            3,
            "a zero NOWAIT reply cannot establish EOF"
        );
        assert_eq!(&bytes[..3], b"abc");
        engine.cache_read_limit = Some(usize::MAX);
        let mut first = [99; 3];
        let mut invalid = [99; 3];
        let mut untouched = [99; 3];
        let results = engine.read(&mut [
            Read {
                file: &file,
                offset: 0,
                buffer: &mut first,
            },
            Read {
                file: &writeonly,
                offset: 0,
                buffer: &mut invalid,
            },
            Read {
                file: &file,
                offset: 0,
                buffer: &mut untouched,
            },
        ]);
        assert_eq!(*results[0].as_ref().unwrap(), 3);
        assert_eq!(
            results[1].as_ref().unwrap_err().raw_os_error(),
            Some(libc::EBADF)
        );
        assert_eq!(*results[2].as_ref().unwrap(), 0);
        assert_eq!(first, *b"abc");
        assert_eq!((invalid, untouched), ([99; 3], [99; 3]));
        assert_eq!(telemetry.snapshot().submissions, 2);
        engine.ring.take();
        assert!(
            engine.read(&mut [Read {
                file: &file,
                offset: 0,
                buffer: &mut untouched
            }])[0]
                .is_err()
        );
        assert_eq!(
            untouched, [99; 3],
            "poisoned executors must not bypass the ring through cache hits"
        );
    }

    #[test]
    fn cooperative_ring_can_move_between_threads_after_draining_each_call() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"old").unwrap();
        let file = Arc::new(File::options().read(true).write(true).open(&path).unwrap());
        let (mut engine, telemetry) = Engine::new(Options::default()).unwrap();
        let (mut engine, file) = std::thread::spawn(move || {
            assert_eq!(
                *engine.write(&[Write {
                    file: &file,
                    offset: 0,
                    data: b"new"
                }])[0]
                    .as_ref()
                    .unwrap(),
                3
            );
            (engine, file)
        })
        .join()
        .unwrap();
        let mut bytes = [0; 3];
        assert_eq!(
            *engine.read(&mut [Read {
                file: &file,
                offset: 0,
                buffer: &mut bytes
            }])[0]
                .as_ref()
                .unwrap(),
            3
        );
        assert_eq!(bytes, *b"new");
        let stats = telemetry.snapshot();
        assert_eq!(
            (stats.waves, stats.submissions, stats.completions),
            (2, 2, 2)
        );
    }

    #[test]
    fn merged_short_writes_preserve_each_requests_contiguous_progress() {
        for short in [1, 3, 4, 6] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("file");
            std::fs::write(&path, b"00000000").unwrap();
            let file = Arc::new(File::options().write(true).open(&path).unwrap());
            let (mut engine, telemetry) = Engine::new(Options::default()).unwrap();
            engine.short_completion = Some(short);
            let results = engine.write(&[
                Write {
                    file: &file,
                    offset: 0,
                    data: b"abc",
                },
                Write {
                    file: &file,
                    offset: 3,
                    data: b"defgh",
                },
            ]);
            assert_eq!(*results[0].as_ref().unwrap(), 3);
            assert_eq!(*results[1].as_ref().unwrap(), 5);
            assert_eq!(std::fs::read(&path).unwrap(), b"abcdefgh");
            let waves = 8usize.div_ceil(short) as u64;
            let stats = telemetry.snapshot();
            assert_eq!(
                (stats.waves, stats.submissions, stats.completions),
                (waves, waves, waves)
            );
        }
    }

    #[test]
    fn merged_short_write_is_not_replayed_when_another_completion_fails() {
        for short in [1, 3, 4, 6] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("file");
            std::fs::write(&path, b"00000000").unwrap();
            let file = Arc::new(File::options().write(true).open(&path).unwrap());
            let readonly = Arc::new(File::open(&path).unwrap());
            let (mut engine, telemetry) = Engine::new(Options::default()).unwrap();
            engine.short_completion = Some(short);
            let results = engine.write(&[
                Write {
                    file: &file,
                    offset: 0,
                    data: b"abc",
                },
                Write {
                    file: &file,
                    offset: 3,
                    data: b"defgh",
                },
                Write {
                    file: &readonly,
                    offset: 0,
                    data: b"bad",
                },
            ]);
            assert_eq!(*results[0].as_ref().unwrap(), short.min(3));
            assert_eq!(*results[1].as_ref().unwrap(), short.saturating_sub(3));
            assert_eq!(
                results[2].as_ref().unwrap_err().raw_os_error(),
                Some(libc::EBADF)
            );
            let mut expected = *b"00000000";
            expected[..short].copy_from_slice(&b"abcdefgh"[..short]);
            assert_eq!(std::fs::read(&path).unwrap(), expected);
            let stats = telemetry.snapshot();
            assert_eq!(
                (stats.waves, stats.submissions, stats.completions),
                (1, 2, 2)
            );
        }
    }

    #[test]
    fn merged_zero_progress_and_failed_writes_never_retry_the_group() {
        for zero_progress in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("file");
            std::fs::write(&path, b"000000").unwrap();
            let file = Arc::new(
                File::options()
                    .read(true)
                    .write(zero_progress)
                    .open(&path)
                    .unwrap(),
            );
            let (mut engine, telemetry) = Engine::new(Options::default()).unwrap();
            engine.short_completion = zero_progress.then_some(0);
            let results = engine.write(&[
                Write {
                    file: &file,
                    offset: 0,
                    data: b"abc",
                },
                Write {
                    file: &file,
                    offset: 3,
                    data: b"DEF",
                },
            ]);
            let errno = if zero_progress {
                libc::EIO
            } else {
                libc::EBADF
            };
            assert!(
                results
                    .iter()
                    .all(|result| result.as_ref().unwrap_err().raw_os_error() == Some(errno))
            );
            assert_eq!(std::fs::read(path).unwrap(), b"000000");
            let stats = telemetry.snapshot();
            assert_eq!(
                (stats.waves, stats.submissions, stats.completions),
                (1, 1, 1)
            );
        }
    }

    #[test]
    fn waves_reuse_bounded_storage_and_wait_once_for_all_completions() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"0123456789").unwrap();
        let file = Arc::new(File::options().read(true).write(true).open(&path).unwrap());
        let (mut engine, telemetry) = Engine::new(
            Options::default().max_batch_bytes(std::num::NonZeroUsize::new(7).unwrap()),
        )
        .unwrap();
        for value in *b"abc" {
            let bytes = [value; 10];
            let results = engine.write(&[Write {
                file: &file,
                offset: 0,
                data: &bytes,
            }]);
            assert_eq!(*results[0].as_ref().unwrap(), 10);
            let mut actual = [0; 10];
            let results = engine.read(&mut [Read {
                file: &file,
                offset: 0,
                buffer: &mut actual,
            }]);
            assert_eq!(*results[0].as_ref().unwrap(), 10);
            assert_eq!(actual, bytes);
            assert!(engine.scratch.capacity() <= 7);
        }
        // Recycled storage must not leak stale bytes past a short/EOF read.
        let mut tail = [99; 7];
        let results = engine.read(&mut [Read {
            file: &file,
            offset: 8,
            buffer: &mut tail,
        }]);
        assert_eq!(*results[0].as_ref().unwrap(), 2);
        assert_eq!(tail, [b'c', b'c', 99, 99, 99, 99, 99]);
        let stats = telemetry.snapshot();
        assert_eq!(stats.scratch_growths, 1);
        assert_eq!((stats.peak_bytes, stats.waves), (7, 13));
        assert_eq!(
            (stats.enters, stats.submissions, stats.completions),
            (13, 13, 13)
        );
    }

    #[test]
    fn many_small_requests_share_one_arena_and_one_enter() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, vec![42; 1024]).unwrap();
        let file = Arc::new(File::open(&path).unwrap());
        let (mut engine, telemetry) = Engine::new(Options::default()).unwrap();
        for _ in 0..3 {
            let mut bytes = vec![[0; 4]; 256];
            let mut operations: Vec<_> = bytes
                .iter_mut()
                .enumerate()
                .map(|(i, buffer)| Read {
                    file: &file,
                    offset: (i * 4) as u64,
                    buffer,
                })
                .collect();
            let results = engine.read(&mut operations);
            assert!(results.iter().all(|result| *result.as_ref().unwrap() == 4));
            assert!(bytes.iter().all(|buffer| buffer == &[42; 4]));
        }
        let stats = telemetry.snapshot();
        assert_eq!(
            (stats.waves, stats.enters, stats.scratch_growths),
            (3, 3, 1)
        );
        assert_eq!((stats.submissions, stats.completions), (768, 768));
        let results = engine.read_owned(
            (0..256)
                .map(|i| OwnedRead {
                    file: Arc::clone(&file),
                    offset: (i * 4) as u64,
                    buffer: vec![0; 4],
                })
                .collect(),
        );
        assert!(
            results
                .iter()
                .all(|result| result.as_ref().unwrap() == &[42; 4])
        );
        let stats = telemetry.snapshot();
        assert_eq!(
            (stats.waves, stats.enters, stats.scratch_growths),
            (4, 4, 1)
        );
        assert_eq!((stats.submissions, stats.completions), (1024, 1024));
        assert_eq!(
            stats.copied_read_bytes,
            3 * 1024,
            "the owned cohort adds no copies"
        );
    }

    #[test]
    fn eof_relative_reads_observe_external_growth_and_truncation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"abc").unwrap();
        let (engine, _) = Engine::new(Options::default()).unwrap();
        let mut fs = vfsi_local::LocalBackend::new(root.path().to_path_buf(), engine).unwrap();
        let file = fs
            .open_raw_impl(std::path::Path::new("/file"), libc::O_RDONLY, 0)
            .unwrap();
        for bytes in [b"abcdefgh".as_slice(), b"x"] {
            std::fs::write(&path, bytes).unwrap();
            let results = fs
                .vread_impl(&[
                    ReadOp {
                        file: file.clone(),
                        offset: VfOffset::At(0),
                        length: 10,
                    },
                    ReadOp {
                        file: file.clone(),
                        offset: VfOffset::End,
                        length: 1,
                    },
                ])
                .unwrap();
            assert_eq!(results[0].data, bytes);
            assert!(results[0].eof);
            assert_eq!(results[1].offset, bytes.len() as u64);
            assert!(results[1].eof);
            assert!(results[1].data.is_empty());
            assert_eq!(
                fs.seek_raw_impl(&file, 0, vfsi_core::SeekFrom::Cur)
                    .unwrap(),
                0
            );
        }
        fs.close_impl(&file).unwrap();
    }

    #[test]
    fn injected_short_writes_complete_contiguous_suffixes_and_short_reads_stop() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"0000000000").unwrap();
        let file = Arc::new(File::options().read(true).write(true).open(&path).unwrap());
        let (mut engine, telemetry) = Engine::new(Options::default()).unwrap();
        engine.short_completion = Some(2);
        assert_eq!(
            engine.write(&[Write {
                file: &file,
                offset: 0,
                data: b"abcdefghij"
            }])[0]
                .as_ref()
                .unwrap(),
            &10
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdefghij");
        assert_eq!(telemetry.snapshot().waves, 5);
        let mut bytes = [99; 10];
        assert_eq!(
            engine.read(&mut [Read {
                file: &file,
                offset: 0,
                buffer: &mut bytes
            }])[0]
                .as_ref()
                .unwrap(),
            &2
        );
        assert_eq!(bytes, [b'a', b'b', 99, 99, 99, 99, 99, 99, 99, 99]);
        assert_eq!(
            telemetry.snapshot().submissions,
            telemetry.snapshot().completions
        );
    }

    #[test]
    fn duplicate_cursor_reads_and_append_remain_ordered() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"abcdef").unwrap();
        let (engine, telemetry) = Engine::new(Options::default()).unwrap();
        let mut fs = vfsi_local::LocalBackend::new(root.path().to_path_buf(), engine).unwrap();
        let file = fs
            .open_raw_impl(std::path::Path::new("/file"), libc::O_RDWR, 0)
            .unwrap();
        let read = ReadOp {
            file: file.clone(),
            offset: VfOffset::Cur,
            length: 2,
        };
        let results = fs.vread_impl(&[read.clone(), read]).unwrap();
        assert_eq!(results[0].data, b"ab");
        assert_eq!(results[1].data, b"cd");
        assert_eq!(telemetry.snapshot().submissions, 0);
        let positional = ReadOp {
            file: file.clone(),
            offset: VfOffset::At(0),
            length: 2,
        };
        fs.vread_impl(&[positional]).unwrap();
        assert_eq!(
            fs.seek_raw_impl(&file, 0, vfsi_core::SeekFrom::Cur)
                .unwrap(),
            4
        );
        let append = fs
            .open_raw_impl(
                std::path::Path::new("/file"),
                libc::O_WRONLY | libc::O_APPEND,
                0,
            )
            .unwrap();
        fs.vwrite_impl(&[WriteOp::new(&append, VfOffset::At(0), b"xy".as_slice())])
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("file")).unwrap(),
            b"abcdefxy"
        );
        fs.close_impl(&file).unwrap();
        fs.close_impl(&append).unwrap();
    }

    #[test]
    fn failure_does_not_finish_another_jobs_short_write() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"000").unwrap();
        let writable = Arc::new(File::options().write(true).open(&path).unwrap());
        let readonly = Arc::new(File::open(&path).unwrap());
        let (mut engine, telemetry) = Engine::new(Options::default()).unwrap();
        engine.short_completion = Some(2);
        let results = engine.write(&[
            Write {
                file: &writable,
                offset: 0,
                data: b"abc",
            },
            Write {
                file: &readonly,
                offset: 0,
                data: b"xyz",
            },
        ]);
        assert_eq!(*results[0].as_ref().unwrap(), 2);
        assert_eq!(
            results[1].as_ref().unwrap_err().raw_os_error(),
            Some(libc::EBADF)
        );
        assert_eq!(std::fs::read(path).unwrap(), b"ab0");
        let stats = telemetry.snapshot();
        assert_eq!(
            (stats.waves, stats.submissions, stats.completions),
            (1, 2, 2)
        );
    }

    #[test]
    fn failure_after_publication_retains_inflight_ownership_without_replay() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"old").unwrap();
        let file = Arc::new(File::options().write(true).open(&path).unwrap());
        let weak = Arc::downgrade(&file);
        let (mut engine, telemetry) = Engine::new(Options::default()).unwrap();
        engine.fail_after_submit = true;
        let results = engine.write(&[
            Write {
                file: &file,
                offset: 0,
                data: b"ne",
            },
            Write {
                file: &file,
                offset: 2,
                data: b"w",
            },
        ]);
        assert!(results.iter().all(Result::is_err));
        assert!(engine.ring.is_none());
        assert_eq!(telemetry.snapshot().submissions, 1);
        assert_eq!(telemetry.snapshot().completions, 0);
        assert!(
            engine.scratch.is_empty(),
            "in-flight arena is retained, not reused"
        );
        assert!(
            engine.write(&[Write {
                file: &file,
                offset: 0,
                data: b"bad"
            }])[0]
                .is_err()
        );
        assert_eq!(telemetry.snapshot().submissions, 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(
            engine.scratch.is_empty(),
            "poisoned calls must not grow a new arena"
        );
        drop(engine);
        drop(file);
        // This exceptional path deliberately retains the bounded wave:
        // dropping the caller/engine must not release kernel-facing storage.
        assert!(weak.upgrade().is_some());
    }

    #[test]
    fn poisoned_executor_does_not_submit_or_replay_writes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"old").unwrap();
        let file = Arc::new(File::options().write(true).open(&path).unwrap());
        let (mut engine, telemetry) = Engine::new(Options::default()).unwrap();
        engine.ring.take();
        assert!(
            engine.write(&[Write {
                file: &file,
                offset: 0,
                data: b"new"
            }])[0]
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert_eq!(telemetry.snapshot().submissions, 0);
    }
}
