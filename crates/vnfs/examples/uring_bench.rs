//! Reproducible warm page-cache comparison; no cache-dropping or host tuning.
#[cfg(target_os = "linux")]
fn pattern_byte(file: usize, offset: u64, generation: u8) -> u8 {
    ((file as u64 * 67 + offset % 251) as u8).wrapping_add(generation)
}

#[cfg(target_os = "linux")]
fn fill_pattern(buffer: &mut [u8], file: usize, offset: u64, generation: u8) {
    for (i, byte) in buffer.iter_mut().enumerate() {
        *byte = pattern_byte(file, offset + i as u64, generation);
    }
}

#[cfg(target_os = "linux")]
fn verify_buffers(
    buffers: &[Vec<u8>],
    offsets: &[u64],
    files: usize,
    generation: u8,
) -> Result<(), &'static str> {
    if files == 0 || buffers.len() != offsets.len() {
        return Err("wrong benchmark result cardinality");
    }
    for (i, (buffer, offset)) in buffers.iter().zip(offsets).enumerate() {
        if buffer
            .iter()
            .enumerate()
            .any(|(j, byte)| *byte != pattern_byte(i % files, offset + j as u64, generation))
        {
            return Err("benchmark contents differ: missing, misrouted, or stale I/O");
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::{
        num::NonZeroUsize, os::fd::AsRawFd, os::unix::fs::FileExt, path::Path, time::Instant,
    };
    use vnfs::{OpenFlags, OpenOp, ReadOp, ReadOptions, Vfsi, VfsiExt, WriteOp, WriteOptions};

    enum Mode {
        Read,
        Write,
        ColdRead,
    }

    fn measure(
        (fs, root): (&impl Vfsi, &Path),
        name: &str,
        paths: &[String],
        offsets: &[u64],
        size: usize,
        rounds: usize,
        mode: Mode,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let write = matches!(mode, Mode::Write);
        let cold = matches!(mode, Mode::ColdRead);
        let files = fs.vopen(
            &paths
                .iter()
                .map(|path| OpenOp::new(path, OpenFlags::READ | OpenFlags::WRITE))
                .collect::<Vec<_>>(),
        )?;
        let mut buffers = vec![vec![0u8; size]; offsets.len()];
        let budget = ReadOptions::new().max_total_bytes(NonZeroUsize::new(size * offsets.len()));
        let mut samples = Vec::new();
        let source: Vec<_> = paths
            .iter()
            .map(|path| {
                std::fs::File::options()
                    .read(true)
                    .write(true)
                    .open(root.join(&path[1..]))
            })
            .collect::<std::io::Result<_>>()?;
        // Reset both backends to identical, distinguishable contents. Source
        // preparation and validation never use the executor being measured.
        for (i, (buffer, offset)) in buffers.iter_mut().zip(offsets).enumerate() {
            fill_pattern(buffer, i % files.len(), *offset, 0);
            source[i % files.len()].write_all_at(buffer, *offset)?;
        }
        if cold {
            for file in &source {
                file.sync_all()?;
            }
        }
        let expected = buffers.clone();
        let mut alternate = Vec::new();
        if write {
            alternate = buffers.clone();
            for (i, (buffer, offset)) in buffers.iter_mut().zip(offsets).enumerate() {
                fill_pattern(buffer, i % files.len(), *offset, 1);
                fill_pattern(&mut alternate[i], i % files.len(), *offset, 2);
            }
        }
        let mut generation = 0;
        for round in 0..rounds + 5 {
            generation = if write { (round % 2 + 1) as u8 } else { 0 };
            // Precompute distinguishable generations, then swap/compare them.
            // Per-byte modulo in preparation/validation used to dominate CPU
            // profiles despite being outside the measured operation.
            if write && round != 0 {
                std::mem::swap(&mut buffers, &mut alternate);
            } else if !write {
                for buffer in &mut buffers {
                    buffer.fill(0xa5);
                }
            }
            for file in source.iter().filter(|_| cold) {
                // Outside the timed region. Advisory eviction is not a
                // guarantee of cold media or eviction of device/controller caches.
                let result = unsafe {
                    libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED)
                };
                if result != 0 {
                    return Err(std::io::Error::from_raw_os_error(result).into());
                }
            }
            let start = Instant::now();
            if write {
                let ops: Vec<_> = offsets
                    .iter()
                    .zip(&buffers)
                    .enumerate()
                    .map(|(i, (offset, data))| WriteOp::at(&files[i % files.len()], *offset, data))
                    .collect();
                let results = fs.vwrite(&ops, WriteOptions::new().write_all(true))?;
                assert_eq!(results.len(), offsets.len());
                assert!(results.iter().all(|result| result.written == size));
            } else {
                let ops: Vec<_> = offsets
                    .iter()
                    .zip(&mut buffers)
                    .enumerate()
                    .map(|(i, (offset, buffer))| {
                        ReadOp::into(&files[i % files.len()], *offset, buffer)
                    })
                    .collect();
                let results = fs.vread(ops, budget)?;
                assert_eq!(results.len(), offsets.len());
                assert!(results.iter().all(|result| result.read() == size));
            }
            let elapsed = start.elapsed().as_secs_f64();
            if !write && buffers != expected {
                return Err("benchmark read contents differ".into());
            }
            if round >= 5 {
                samples.push(elapsed);
            }
        }
        // Verify actual contents outside the timed region, not just throughput.
        if write {
            for (i, (buffer, offset)) in buffers.iter_mut().zip(offsets).enumerate() {
                fill_pattern(buffer, i % files.len(), *offset, generation.wrapping_add(1));
                source[i % files.len()].read_exact_at(buffer, *offset)?;
            }
        }
        verify_buffers(&buffers, offsets, files.len(), generation)?;
        fs.close_files(files)?;
        samples.sort_by(f64::total_cmp);
        let median = samples[samples.len() / 2];
        println!(
            "{name}: median_us={:.1} MiB_s={:.1}",
            median * 1e6,
            size as f64 * offsets.len() as f64 / (1024.0 * 1024.0 * median)
        );
        Ok(())
    }

    let rounds = std::env::args()
        .nth(1)
        .map(|arg| arg.parse())
        .transpose()?
        .unwrap_or(100usize);
    if rounds == 0 {
        return Err("rounds must be nonzero".into());
    }
    // /tmp is often tmpfs; use disk-backed build storage by default. A second
    // argument can select another existing scratch parent/filesystem.
    let parent = std::env::args().nth(2).unwrap_or_else(|| "target".into());
    let backend = std::env::args().nth(3).unwrap_or_else(|| "both".into());
    let phase = std::env::args().nth(4).unwrap_or_else(|| "all".into());
    if !matches!(backend.as_str(), "both" | "posix" | "uring")
        || !matches!(phase.as_str(), "all" | "warm" | "cold")
    {
        return Err(
            "usage: uring_bench [rounds] [parent] [both|posix|uring] [all|warm|cold] [queue_depth]"
                .into(),
        );
    }
    let depth = std::env::args()
        .nth(5)
        .map(|arg| arg.parse::<u32>())
        .transpose()?
        .unwrap_or(256);
    let options = vfsi_uring::Options::default()
        .queue_depth(std::num::NonZeroU32::new(depth).ok_or("queue_depth must be nonzero")?);
    let temp = tempfile::Builder::new()
        .prefix("vfsi-uring-bench-")
        .tempdir_in(parent)?;
    let root = temp.path();
    let paths: Vec<_> = (0..256).map(|i| format!("/file-{i}")).collect();
    for path in &paths {
        std::fs::write(root.join(&path[1..]), vec![0; 4096])?;
    }
    std::fs::write(root.join("large"), vec![0; 16 * 1024 * 1024])?;
    for i in 0..16 {
        let dir = root.join(format!("dir-{i}"));
        std::fs::create_dir(&dir)?;
        for j in 0..16 {
            std::fs::write(dir.join(format!("file-{j}")), b"directory")?;
        }
    }
    let local = vfsi_posix::connect(root)?;
    // Identical FsClient adapters: compare execution engines rather than an
    // opaque facade on one side and a native adapter on the other.
    let (uring, telemetry) = vfsi_uring::connect_with_telemetry(root, options)?;
    println!(
        "warm-cache, buffered I/O, no fsync, {} measured rounds; root={}",
        rounds,
        root.display()
    );
    fn cases(
        fs: &impl Vfsi,
        root: &Path,
        name: &str,
        paths: &[String],
        rounds: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let offsets = vec![0; paths.len()];
        measure(
            (fs, root),
            &format!("{name}/small-read"),
            paths,
            &offsets,
            4096,
            rounds,
            Mode::Read,
        )?;
        measure(
            (fs, root),
            &format!("{name}/small-write"),
            paths,
            &offsets,
            4096,
            rounds,
            Mode::Write,
        )?;
        let offsets: Vec<_> = (0..128).map(|i| i * 128 * 1024).collect();
        measure(
            (fs, root),
            &format!("{name}/large-read"),
            &["/large".into()],
            &offsets,
            128 * 1024,
            rounds,
            Mode::Read,
        )?;
        measure(
            (fs, root),
            &format!("{name}/large-write"),
            &["/large".into()],
            &offsets,
            128 * 1024,
            rounds,
            Mode::Write,
        )?;
        let dirs: Vec<_> = (0..16).map(|i| format!("/dir-{i}")).collect();
        let start = Instant::now();
        for _ in 0..rounds {
            let listings = fs.read_dirs(&dirs)?;
            assert_eq!(listings.len(), dirs.len());
            for listing in listings {
                assert_eq!(listing.entries.len(), 16);
            }
        }
        println!(
            "{name}/directories: mean_us={:.1}",
            start.elapsed().as_secs_f64() * 1e6 / rounds as f64
        );
        Ok(())
    }
    if phase != "cold" {
        if backend != "uring" {
            cases(&local, root, "local", &paths, rounds)?;
        }
        if backend != "posix" {
            cases(&uring, root, "uring", &paths, rounds)?;
        }
    }
    if phase == "warm" {
        let stats = telemetry.snapshot();
        println!("ring: {stats:?}");
        assert_eq!(stats.submissions, stats.completions);
        return Ok(());
    }
    println!("advisory page-cache eviction before each read; sync/eviction outside timing");
    let cold_rounds = rounds.min(20);
    let offsets = vec![0; paths.len()];
    if backend != "uring" {
        measure(
            (&local, root),
            "local/cold-small-read",
            &paths,
            &offsets,
            4096,
            cold_rounds,
            Mode::ColdRead,
        )?;
    }
    if backend != "posix" {
        measure(
            (&uring, root),
            "uring/cold-small-read",
            &paths,
            &offsets,
            4096,
            cold_rounds,
            Mode::ColdRead,
        )?;
    }
    let offsets: Vec<_> = (0..128).map(|i| i * 128 * 1024).collect();
    if backend != "uring" {
        measure(
            (&local, root),
            "local/cold-large-read",
            &["/large".into()],
            &offsets,
            128 * 1024,
            cold_rounds,
            Mode::ColdRead,
        )?;
    }
    if backend != "posix" {
        measure(
            (&uring, root),
            "uring/cold-large-read",
            &["/large".into()],
            &offsets,
            128 * 1024,
            cold_rounds,
            Mode::ColdRead,
        )?;
    }
    let stats = telemetry.snapshot();
    println!(
        "ring: waves={} submissions={} completions={} peak_scratch_bytes={} enters={} scratch_growths={}",
        stats.waves,
        stats.submissions,
        stats.completions,
        stats.peak_bytes,
        stats.enters,
        stats.scratch_growths
    );
    assert_eq!(stats.submissions, stats.completions);
    if backend != "posix" && depth > 1 {
        assert!(stats.submissions > stats.waves);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("io_uring is Linux-only");
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_noop_reads_and_unchanged_writes() {
        let mut buffers = vec![vec![0; 32]; 2];
        let offsets = [0, 256];
        for (i, buffer) in buffers.iter_mut().enumerate() {
            fill_pattern(buffer, i, offsets[i], 1);
        }
        assert!(verify_buffers(&buffers, &offsets, 2, 0).is_err());
        // Existing generation zero bytes cannot validate a no-op write.
        assert!(verify_buffers(&buffers, &offsets, 2, 2).is_err());
        assert!(verify_buffers(&buffers, &offsets, 2, 1).is_ok());
    }

    #[test]
    fn validation_rejects_wrong_files_offsets_and_missing_results() {
        let mut buffers = vec![vec![0; 32]; 2];
        let offsets = [0, 128 * 1024];
        for (i, buffer) in buffers.iter_mut().enumerate() {
            fill_pattern(buffer, i, offsets[i], 0);
        }
        assert!(verify_buffers(&buffers, &offsets, 2, 0).is_ok());
        buffers.swap(0, 1);
        assert!(verify_buffers(&buffers, &offsets, 2, 0).is_err());
        buffers.swap(0, 1);
        assert!(verify_buffers(&buffers, &[1, offsets[1]], 2, 0).is_err());
        assert!(verify_buffers(&buffers[..1], &offsets, 2, 0).is_err());
    }
}
