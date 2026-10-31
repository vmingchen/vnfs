//! Reproducible buffered-I/O comparison; only advisory per-file cache eviction.
#[cfg(target_os = "linux")]
#[path = "uring_bench/support.rs"]
mod support;

#[cfg(target_os = "linux")]
enum Mode {
    Read,
    Write,
    ColdRead,
    MixedRead,
    OwnedRead(bool),
    WholeRead(bool),
}

#[cfg(target_os = "linux")]
struct Case {
    name: &'static str,
    paths: Vec<String>,
    offsets: Vec<u64>,
    size: usize,
    last_size: Option<usize>,
    mode: Mode,
}
#[cfg(target_os = "linux")]
impl Case {
    fn length(&self, index: usize) -> usize {
        if index + 1 == self.offsets.len() {
            self.last_size.unwrap_or(self.size)
        } else {
            self.size
        }
    }

    fn bytes(&self) -> usize {
        self.size * self.offsets.len().saturating_sub(1) + self.last_size.unwrap_or(self.size)
    }
}
#[cfg(target_os = "linux")]
fn pattern_byte(file: usize, offset: u64, generation: u8) -> u8 {
    // Mix the full file index and offset: simple byte arithmetic made files
    // 0 and 256 indistinguishable when scaling beyond the original fixture.
    let value = (file as u64).wrapping_mul(0x9e3779b97f4a7c15) ^ offset;
    let value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    ((value ^ (value >> 27)) as u8).wrapping_add(generation)
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
fn run(config: support::Config) -> Result<(), Box<dyn std::error::Error>> {
    use std::{
        num::NonZeroUsize, os::fd::AsRawFd, os::unix::fs::FileExt, path::Path, time::Instant,
    };
    use vnfs::files::{
        OpenFlags, OpenOp, ReadOp, ReadOptions, Vfsi, VfsiExt, WriteOp, WriteOptions,
    };

    fn measure(
        (fs, root): (&impl Vfsi, &Path),
        backend: &str,
        case: &Case,
        rounds: usize,
        report: &mut support::Report,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Case {
            paths,
            offsets,
            mode,
            ..
        } = case;
        let lengths: Vec<_> = (0..offsets.len()).map(|index| case.length(index)).collect();
        let write = matches!(mode, Mode::Write);
        let cold = matches!(
            mode,
            Mode::ColdRead | Mode::MixedRead | Mode::OwnedRead(true) | Mode::WholeRead(true)
        );
        let owned = matches!(mode, Mode::OwnedRead(_) | Mode::WholeRead(_));
        let whole = matches!(mode, Mode::WholeRead(_));
        let mixed = matches!(mode, Mode::MixedRead);
        let file_count = paths.len();
        // Whole-file calls open their own bounded cohort inside the timed
        // operation. Retaining another unused set here inflates FD usage.
        let files = if whole {
            Vec::new()
        } else {
            fs.vopen(
                &paths
                    .iter()
                    .map(|path| OpenOp::new(path, OpenFlags::READ | OpenFlags::WRITE))
                    .collect::<Vec<_>>(),
            )?
        };
        let mut buffers: Vec<_> = lengths.iter().map(|&length| vec![0u8; length]).collect();
        let budget =
            ReadOptions::new().with_max_total_bytes(NonZeroUsize::new(case.bytes()).unwrap());
        let mut samples = Vec::new();
        let mut cpu = 0.0;
        let mut before = report.snapshot();
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
        report.preparation_bytes = 0;
        if whole {
            for (file, &length) in source.iter().zip(&lengths) {
                // A preceding write-shape case may have extended a small file.
                // Whole-file phases must also validate/truncate its old tail.
                if file.metadata()?.len() != length as u64 {
                    file.set_len(length as u64)?;
                }
            }
        }
        let mut current = Vec::new();
        for (i, (buffer, offset)) in buffers.iter_mut().zip(offsets).enumerate() {
            current.resize(buffer.len(), 0);
            fill_pattern(buffer, i % file_count, *offset, 0);
            let source = &source[i % file_count];
            // Reused read fixtures must not be rewritten: that can change
            // physical extents on copy-on-write filesystems between binaries.
            if source.read_at(&mut current, *offset)? != buffer.len() || current != *buffer {
                source.write_all_at(buffer, *offset)?;
                report.preparation_bytes += buffer.len() as u64;
            }
        }
        drop(current);
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
                fill_pattern(buffer, i % file_count, *offset, 1);
                fill_pattern(&mut alternate[i], i % file_count, *offset, 2);
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
            for file in source.iter().skip(usize::from(mixed)).filter(|_| cold) {
                // Outside the timed region. Advisory eviction is not a
                // guarantee of cold media or eviction of device/controller caches.
                // SAFETY: file owns the descriptor; posix_fadvise takes no pointers
                // and receives a valid whole-file range and advisory constant.
                let result = unsafe {
                    libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED)
                };
                if result != 0 {
                    return Err(std::io::Error::from_raw_os_error(result).into());
                }
            }
            if round == 5 {
                before = report.snapshot();
            }
            let cpu_start = support::cpu_seconds()?;
            let start = Instant::now();
            if write {
                let ops: Vec<_> = offsets
                    .iter()
                    .zip(&buffers)
                    .enumerate()
                    .map(|(i, (offset, data))| WriteOp::at(&files[i % file_count], *offset, data))
                    .collect();
                let results = fs.vwrite(&ops, WriteOptions::new().with_write_all(true))?;
                assert_eq!(results.len(), offsets.len());
                assert!(
                    results
                        .iter()
                        .zip(&lengths)
                        .all(|(result, &length)| result.bytes_written == length)
                );
            } else if owned {
                let results = if whole {
                    fs.vread(paths.iter().map(ReadOp::whole), budget)?
                } else {
                    fs.vread(
                        offsets.iter().enumerate().map(|(i, offset)| {
                            ReadOp::range(&files[i % file_count], *offset, lengths[i])
                        }),
                        budget,
                    )?
                };
                assert_eq!(results.len(), offsets.len());
                assert!(
                    results
                        .iter()
                        .zip(&lengths)
                        .all(|(result, &length)| result.bytes_read() == length)
                );
                buffers = results
                    .into_iter()
                    .map(|result| result.into_data().unwrap())
                    .collect();
            } else {
                let ops: Vec<_> = offsets
                    .iter()
                    .zip(&mut buffers)
                    .enumerate()
                    .map(|(i, (offset, buffer))| {
                        ReadOp::into(&files[i % file_count], *offset, buffer)
                    })
                    .collect();
                let results = fs.vread(ops, budget)?;
                assert_eq!(results.len(), offsets.len());
                assert!(
                    results
                        .iter()
                        .zip(&lengths)
                        .all(|(result, &length)| result.bytes_read() == length)
                );
            }
            let elapsed = start.elapsed().as_nanos() as u64;
            let used_cpu = support::cpu_seconds()? - cpu_start;
            if !write && buffers != expected {
                return Err("benchmark read contents differ".into());
            }
            if round >= 5 {
                samples.push(elapsed);
                cpu += used_cpu;
            }
        }
        let after = report.snapshot();
        // Verify actual contents outside the timed region, not just throughput.
        if write {
            for (i, (buffer, offset)) in buffers.iter_mut().zip(offsets).enumerate() {
                fill_pattern(buffer, i % file_count, *offset, generation.wrapping_add(1));
                source[i % file_count].read_exact_at(buffer, *offset)?;
            }
        }
        verify_buffers(&buffers, offsets, file_count, generation)?;
        fs.close_files(files)?;
        report.record(backend, case, (samples, cpu), (before, after));
        Ok(())
    }

    let cases = config.cases()?;
    support::check_fd_limit(&cases)?;
    let fixture = support::Fixture::open(&config)?;
    let root = fixture.root.as_path();
    let mut report = support::Report::new(&config, root)?;
    report.fixture(&fixture);
    println!("{config:?}\nBuffered I/O, no fsync; eviction is advisory; five warmups per case.");
    for case in cases {
        for backend in config.backends() {
            // Fresh executors make peak/scratch counters case-local, not
            // inherited from an earlier workload. Setup stays outside timing.
            let fs = if backend == "posix" {
                report.telemetry = None;
                vfsi_posix::connect(root)?
            } else {
                let options = vfsi_uring::Options::default()
                    .with_cached_reads(config.execution != "ring")
                    .with_syscall_writes(config.execution == "adaptive")
                    .with_queue_depth(std::num::NonZeroU32::new(config.depth).unwrap())
                    .with_max_batch_bytes(NonZeroUsize::new(config.window).unwrap());
                let (fs, telemetry) = vfsi_uring::connect_with_telemetry(root, options)?;
                report.telemetry = Some(telemetry);
                fs
            };
            let rounds = config.rounds_for(&case);
            if case.name == "directories" {
                report.preparation_bytes = 0;
                let mut samples = Vec::with_capacity(rounds);
                let mut cpu = 0.0;
                let mut before = report.snapshot();
                for round in 0..rounds + 5 {
                    if round == 5 {
                        before = report.snapshot();
                    }
                    let cpu_start = support::cpu_seconds()?;
                    let start = Instant::now();
                    let listings = fs.read_dirs(&case.paths)?;
                    let elapsed = start.elapsed().as_nanos() as u64;
                    let used_cpu = support::cpu_seconds()? - cpu_start;
                    assert_eq!(listings.len(), case.paths.len());
                    assert!(listings.iter().all(|listing| listing.entries.len() == 16));
                    if round >= 5 {
                        samples.push(elapsed);
                        cpu += used_cpu;
                    }
                }
                report.record(backend, &case, (samples, cpu), (before, report.snapshot()));
            } else {
                measure((&fs, root), backend, &case, rounds, &mut report)?;
            }
        }
    }
    report.finish(&config)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|arg| arg == "--help") {
        println!("{}", support::USAGE);
        return Ok(());
    }
    run(support::Config::parse(std::env::args().skip(1))?)
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("io_uring is Linux-only");
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn mixed_size_cases_validate_each_file_and_report_actual_geometry() {
        let parent = tempfile::tempdir().unwrap();
        for files in [1, 3] {
            let output = parent.path().join(format!("mixed-{files}.json"));
            let config = support::Config::parse(
                [
                    "1",
                    parent.path().to_str().unwrap(),
                    "both",
                    "whole-mixed-size-read",
                    "4",
                    "8192",
                    "ring",
                    "--files",
                    &files.to_string(),
                    "--small-bytes",
                    "16",
                    "--large-bytes",
                    "8192",
                    "--chunk-bytes",
                    "4096",
                    "--json",
                    output.to_str().unwrap(),
                ]
                .into_iter()
                .map(str::to_owned),
            )
            .unwrap();
            run(config).unwrap();
            let report: serde_json::Value =
                serde_json::from_reader(std::fs::File::open(output).unwrap()).unwrap();
            for case in report["cases"].as_array().unwrap() {
                assert_eq!(case["bytes_per_call"], 8192 + (files - 1) * 16);
                assert_eq!(
                    case["min_request_bytes"],
                    if files == 1 { 8192 } else { 16 }
                );
                assert_eq!(case["max_request_bytes"], 8192);
                if files == 1 {
                    assert_eq!(case["request_bytes"], 8192);
                } else {
                    assert!(case["request_bytes"].is_null());
                }
            }
        }
        assert!(
            support::Config::parse(
                [
                    "1",
                    parent.path().to_str().unwrap(),
                    "uring",
                    "whole-mixed-size-read",
                    "4",
                    "8192",
                    "ring",
                    "--files",
                    "2",
                    "--small-bytes",
                    "16",
                    "--large-bytes",
                    "8192",
                    "--chunk-bytes",
                    "4096",
                    "--max-case-bytes",
                    "8192",
                ]
                .into_iter()
                .map(str::to_owned)
            )
            .is_err(),
            "include the large tail in the preflight byte budget"
        );
    }

    #[test]
    fn whole_file_benchmark_fits_preflight_descriptor_budget() {
        const CHILD: &str = "VFSI_URING_BENCH_FD_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::whole_file_benchmark_fits_preflight_descriptor_budget",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "descriptor-limited child failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        // SAFETY: getrlimit initializes the valid output pointer on success.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) },
            0
        );
        // SAFETY: the successful getrlimit call initialized limit.
        let mut limit = unsafe { limit.assume_init() };
        limit.rlim_cur = limit.rlim_cur.min(600);
        // SAFETY: only this isolated child test's soft limit is reduced; the
        // hard limit and other tests' descriptor budgets remain unchanged.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let files = ((limit.rlim_cur.saturating_sub(32)) / 2).min(256);
        assert!(files >= 32, "descriptor limit too low to exercise batching");
        let parent = tempfile::tempdir().unwrap();
        let output = parent.path().join("whole.json");
        let files_arg = files.to_string();
        let config = support::Config::parse(
            [
                "1",
                parent.path().to_str().unwrap(),
                "uring",
                "whole-read",
                "256",
                "2097152",
                "ring",
                "--files",
                &files_arg,
                "--json",
                output.to_str().unwrap(),
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        run(config).unwrap();
        let report: serde_json::Value =
            serde_json::from_reader(std::fs::File::open(output).unwrap()).unwrap();
        let cases = report["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0]["rounds"], 1);
        assert_eq!(cases[0]["allocation"], "whole-file-owned");
        let stats = &cases[0]["telemetry"];
        assert!(stats["submissions"].as_u64().unwrap() > 0);
        assert_eq!(stats["submissions"], stats["completions"]);
    }

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

    #[test]
    fn patterns_distinguish_large_file_indices_and_old_offset_periods() {
        let pattern = |file, offset| {
            let mut buffer = vec![0; 32];
            fill_pattern(&mut buffer, file, offset, 0);
            buffer
        };
        assert_ne!(pattern(0, 0), pattern(256, 0));
        assert_ne!(pattern(0, 0), pattern(0, 251));
    }

    #[test]
    fn paired_read_runs_reuse_inodes_without_rewriting_and_honor_cold_rounds() {
        let parent = tempfile::tempdir().unwrap();
        let fixture = parent.path().join("fixture");
        let mut identity = None;
        for index in 0..2 {
            let output = parent.path().join(format!("run-{index}.json"));
            let config = support::Config::parse(
                [
                    "1",
                    parent.path().to_str().unwrap(),
                    "both",
                    "cold-small-read",
                    "2",
                    "8192",
                    "ring",
                    "--files",
                    "4",
                    "--fixture",
                    fixture.to_str().unwrap(),
                    "--cold-rounds",
                    "21",
                    "--json",
                    output.to_str().unwrap(),
                ]
                .into_iter()
                .map(str::to_owned),
            )
            .unwrap();
            run(config).unwrap();
            let report: serde_json::Value =
                serde_json::from_reader(std::fs::File::open(output).unwrap()).unwrap();
            if let Some(prior) = &identity {
                assert_eq!(&report["environment"]["fixture_objects"], prior);
            }
            identity = Some(report["environment"]["fixture_objects"].clone());
            for case in report["cases"].as_array().unwrap() {
                assert_eq!(case["rounds"], 21);
                if index == 1 {
                    assert_eq!(case["preparation_written_bytes"], 0);
                }
            }
            let stats = &report["cases"][1]["telemetry"];
            assert_eq!(stats["submissions"], 84);
            assert_eq!(stats["completions"], 84);
            assert_eq!(stats["waves"], 42);
        }
    }

    #[test]
    fn complete_small_suite_executes_both_backends_and_reports_real_ring_work() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("complete.json");
        let config = support::Config::parse(
            [
                "2",
                temp.path().to_str().unwrap(),
                "both-reverse",
                "all",
                "2",
                "8192",
                "ring",
                "--files",
                "8",
                "--small-bytes",
                "4096",
                "--large-bytes",
                "65536",
                "--chunk-bytes",
                "4096",
                "--order",
                "random",
                "--seed",
                "42",
                "--json",
                path.to_str().unwrap(),
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        run(config).unwrap();
        let report: serde_json::Value =
            serde_json::from_reader(std::fs::File::open(path).unwrap()).unwrap();
        let cases = report["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 40);
        for pair in cases.chunks_exact(2) {
            assert_eq!(pair[0]["case"], pair[1]["case"]);
            assert_eq!(pair[0]["backend"], "uring");
            assert_eq!(pair[1]["backend"], "posix");
            assert!(pair[1]["telemetry"].is_null());
            for case in pair {
                assert_eq!(case["rounds"], 2);
                assert_eq!(case["samples_ns"].as_array().unwrap().len(), 2);
                assert!(case["p50_ns"].as_u64().unwrap() > 0);
                if case["case"].as_str().unwrap().contains("mixed-size") {
                    assert_eq!(case["bytes_per_call"], 7 * 4096 + 65536);
                    assert!(case["request_bytes"].is_null());
                    assert_eq!(case["min_request_bytes"], 4096);
                    assert_eq!(case["max_request_bytes"], 65536);
                }
                if case["case"].as_str().unwrap().starts_with("whole-") {
                    assert_eq!(case["allocation"], "whole-file-owned");
                    assert_eq!(case["path_open_close_timing"], "included");
                    let cold = case["case"].as_str().unwrap().contains("cold");
                    assert_eq!(
                        case["cache"],
                        if cold { "advisory-evicted" } else { "warm" }
                    );
                }
            }
            let stats = &pair[0]["telemetry"];
            assert_eq!(stats["submissions"], stats["completions"]);
            assert!(stats["case_peak_bytes_including_warmup"].as_u64().unwrap() <= 8192);
            if pair[0]["case"] != "directories" {
                assert!(stats["submissions"].as_u64().unwrap() > 0);
                assert!(stats["waves"].as_u64().unwrap() > 0);
            }
            if pair[0]["case"] == "small-read" {
                // Eight reads, two per wave, two measured rounds; five warmups
                // must not leak into the per-case measured counters.
                assert_eq!(stats["submissions"], 16);
                assert_eq!(stats["waves"], 8);
            }
        }
    }
}
