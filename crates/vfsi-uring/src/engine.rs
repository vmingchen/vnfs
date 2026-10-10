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
use vfsi_local::io::{DescriptorIo, Read, Write};

#[derive(Default)]
struct Counters {
    waves: AtomicU64,
    submissions: AtomicU64,
    completions: AtomicU64,
    peak_bytes: AtomicU64,
}

/// Cumulative execution counters; a snapshot may span an ongoing operation.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub waves: u64,
    pub submissions: u64,
    pub completions: u64,
    pub peak_bytes: u64,
}

#[derive(Clone, Default)]
pub struct Telemetry(Arc<Counters>);

impl Telemetry {
    pub fn snapshot(&self) -> Stats {
        let counters = &self.0;
        Stats {
            waves: counters.waves.load(Ordering::Relaxed),
            submissions: counters.submissions.load(Ordering::Relaxed),
            completions: counters.completions.load(Ordering::Relaxed),
            peak_bytes: counters.peak_bytes.load(Ordering::Relaxed),
        }
    }
}

pub(crate) struct Engine {
    ring: Option<IoUring>,
    options: Options,
    telemetry: Telemetry,
    #[cfg(test)]
    short_completion: Option<usize>,
    #[cfg(test)]
    fail_after_submit: bool,
}

enum Buffer<'a> {
    Read(&'a mut [u8]),
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
    // Own everything addressed by an SQE, including the descriptor. Caller
    // buffers are never referenced by the kernel and can safely be released
    // even if an unrecoverable ring failure prevents draining completions.
    file: Arc<File>,
    bytes: Vec<u8>,
    offset: u64,
    kind: Kind,
    job: usize,
}

impl Engine {
    pub(crate) fn new(options: Options) -> io::Result<(Self, Telemetry)> {
        if options.queue_depth.get() > 4096 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let ring = IoUring::new(options.queue_depth.get())?;
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
                #[cfg(test)]
                short_completion: None,
                #[cfg(test)]
                fail_after_submit: false,
            },
            telemetry,
        ))
    }

    fn wave(&mut self, mut pending: Vec<Pending>) -> io::Result<Vec<(Pending, i32)>> {
        let Some(ring) = self.ring.as_mut() else {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        };
        // Complete all allocation and fallible preparation before publishing
        // any pointers into the SQ. Vector storage is not moved/reallocated
        // until every CQE has arrived.
        let entries: Vec<_> = pending
            .iter_mut()
            .enumerate()
            .map(|(index, op)| {
                let fd = types::Fd(op.file.as_raw_fd());
                let length = op.bytes.len() as u32;
                // Inject an actual short transfer, not a fabricated CQE after
                // the full payload has already reached the file.
                #[cfg(test)]
                let length = self.short_completion.map_or(length, |limit| {
                    length.min(u32::try_from(limit).unwrap_or(u32::MAX))
                });
                let entry = match op.kind {
                    Kind::Read => opcode::Read::new(fd, op.bytes.as_mut_ptr(), length)
                        .offset(op.offset)
                        .build(),
                    Kind::Write => opcode::Write::new(fd, op.bytes.as_ptr(), length)
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
            })
            .collect();
        let mut results = vec![None; entries.len()];
        let bytes: usize = pending.iter().map(|op| op.bytes.len()).sum();
        let counters = &self.telemetry.0;
        counters
            .peak_bytes
            .fetch_max(bytes as u64, Ordering::Relaxed);
        counters.waves.fetch_add(1, Ordering::Relaxed);
        let result = (|| {
            {
                let mut queue = ring.submission();
                for entry in &entries {
                    // SAFETY: owned buffers/files are retained until completion;
                    // this empty ring has capacity >= the bounded wave length.
                    unsafe { queue.push(entry) }
                        .map_err(|_| io::Error::from_raw_os_error(libc::EIO))?;
                }
            }
            let mut remaining = entries.len();
            while remaining != 0 {
                match ring.submit_and_wait(1) {
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
                    results[index] = Some(completion.result());
                    remaining -= 1;
                    counters.completions.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            // Ring teardown alone is not proof that kernel references are gone.
            // Poison the executor, never replay writes, and deliberately retain
            // this bounded wave's resources on catastrophic submission failure.
            // Normal negative CQEs are drained and do NOT leak resources.
            self.ring.take();
            std::mem::forget(pending);
            return Err(error);
        }
        Ok(pending
            .into_iter()
            .zip(results)
            .map(|(op, result)| (op, result.unwrap()))
            .collect())
    }

    fn execute(&mut self, jobs: &mut [Job<'_>]) -> Vec<io::Result<usize>> {
        let mut progress = vec![0usize; jobs.len()];
        let mut done = vec![false; jobs.len()];
        let mut errors = vec![None; jobs.len()];
        while done.iter().any(|finished| !finished) {
            let mut pending = Vec::new();
            let mut budget = self.options.max_batch_bytes.get();
            for (index, job) in jobs.iter().enumerate() {
                if done[index] {
                    continue;
                }
                if pending.len() == self.options.queue_depth.get() as usize {
                    break;
                }
                let (kind, length) = match &job.buffer {
                    Buffer::Read(buffer) => (Kind::Read, buffer.len()),
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
                let bytes = match &job.buffer {
                    Buffer::Write(data) => data[progress[index]..progress[index] + count].to_vec(),
                    _ => vec![0; count],
                };
                budget -= count;
                pending.push(Pending {
                    file,
                    bytes,
                    offset,
                    kind,
                    job: index,
                });
            }
            if pending.is_empty() {
                continue;
            }
            match self.wave(pending) {
                Ok(completed) => {
                    for (op, result) in completed {
                        let index = op.job;
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
                            buffer[progress[index]..progress[index] + count]
                                .copy_from_slice(&op.bytes[..count]);
                        }
                        progress[index] += count;
                        // A short read ends this operation; no fixed-offset gap
                        // concatenation. Short writes retry the contiguous suffix.
                        done[index] = match op.kind {
                            Kind::Read => count < op.bytes.len(),
                            Kind::Write if count == 0 => {
                                errors[index] = Some(libc::EIO);
                                true
                            }
                            Kind::Write => false,
                            Kind::Sync(_) => true,
                        };
                    }
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
        progress
            .into_iter()
            .zip(errors)
            .map(|(count, error)| {
                error.map_or(Ok(count), |errno| Err(io::Error::from_raw_os_error(errno)))
            })
            .collect()
    }
}

impl DescriptorIo for Engine {
    fn read(&mut self, operations: &mut [Read<'_>]) -> Vec<io::Result<usize>> {
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
        let mut fs = vfsi_local::DummyVecFs::try_new(root.path().to_path_buf())
            .unwrap()
            .with_io_engine(engine);
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
        let op = Write {
            file: &file,
            offset: 0,
            data: b"new",
        };
        assert!(engine.write(&[op])[0].is_err());
        assert!(engine.ring.is_none());
        assert_eq!(telemetry.snapshot().submissions, 1);
        assert_eq!(telemetry.snapshot().completions, 0);
        assert!(
            engine.write(&[Write {
                file: &file,
                offset: 0,
                data: b"bad"
            }])[0]
                .is_err()
        );
        assert_eq!(telemetry.snapshot().submissions, 1);
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
