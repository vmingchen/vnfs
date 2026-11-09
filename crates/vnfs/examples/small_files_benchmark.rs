//! Compare vnfs vector I/O with scalar std::fs calls over a kernel NFS mount.
//! Defaults to the public API; --api backend or both isolates native execution.
//! All read payload validation happens after the measured interval.

use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use vfsi_sync::backend::VectorBackend;

use vfsi_nfs::NfsVecFs;
use vfsi_sync::{ReadOp, VfOffset, WriteOp};
use vnfs::files::VfsiExt;
use vnfs::nfs::{Nfs, NfsClient, NfsVersion};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Api {
    Public,
    Backend,
}
impl Api {
    fn name(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Backend => "backend",
        }
    }
}

enum BenchmarkFs {
    Public(NfsClient),
    Backend(Box<NfsVecFs>),
}
impl BenchmarkFs {
    fn connect(host: &str, api: Api) -> Result<Self> {
        Ok(match api {
            Api::Public => Self::Public(Nfs::builder(host).version(NfsVersion::V4_2).connect()?),
            Api::Backend => Self::Backend(Box::new(NfsVecFs::connect_minor(host, 2)?)),
        })
    }
    fn write(&mut self, paths: &[String], payload: &[u8]) -> Result<()> {
        match self {
            Self::Public(fs) => {
                let entries: Vec<_> = paths.iter().map(|path| (path, payload)).collect();
                fs.write_files(&entries)?;
            }
            Self::Backend(fs) => {
                let files: Vec<_> = paths
                    .iter()
                    .map(|path| vfsi_sync::FileOperand::from_path(path))
                    .collect();
                let operations: Vec<_> = files
                    .iter()
                    .map(|file| {
                        WriteOp::new(file, VfOffset::At(0), payload)
                            .with_creation()
                            .with_truncate()
                    })
                    .collect();
                let results = fs.vwrite_impl(&operations).and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                })?;
                if results.len() != paths.len()
                    || results.iter().any(|r| r.bytes_written != payload.len())
                {
                    return Err(
                        io::Error::other("native backend returned incomplete writes").into(),
                    );
                }
            }
        }
        Ok(())
    }
    fn read(&mut self, paths: &[String], length: usize) -> Result<Vec<Vec<u8>>> {
        Ok(match self {
            Self::Public(fs) => fs
                .read_batch(
                    paths.iter().map(vnfs::files::ReadOp::whole),
                    Default::default(),
                )?
                .into_iter()
                .map(|result| {
                    result
                        .into_data()
                        .ok_or_else(|| io::Error::other("missing owned read payload"))
                })
                .collect::<io::Result<_>>()?,
            Self::Backend(fs) => {
                let operations: Vec<_> = paths
                    .iter()
                    .map(|path| ReadOp::from_path(path, VfOffset::At(0), length))
                    .collect();
                fs.vread_impl(&operations)
                    .and_then(|outcomes| {
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })?
                    .into_iter()
                    .map(|result| result.data)
                    .collect()
            }
        })
    }
}

#[derive(Debug)]
struct Args {
    host: String,
    remote_root: String,
    direct_root: PathBuf,
    mount_root: PathBuf,
    files: usize,
    bytes: usize,
    rounds: usize,
    reuse_paths: bool,
    apis: &'static [Api],
}

impl Default for Args {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            remote_root: String::new(),
            direct_root: "/export".into(),
            mount_root: "/mnt/nfs".into(),
            files: 20,
            bytes: 4096,
            rounds: 30,
            reuse_paths: false,
            apis: &[Api::Public],
        }
    }
}

#[derive(Debug)]
struct Sample {
    milliseconds: f64,
    compounds: u64,
    rpcs: u64,
}

fn argument_value(arguments: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    arguments.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{name} needs a value")).into()
    })
}

fn parse_args(arguments: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut parsed = Args::default();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--host" => parsed.host = argument_value(&mut arguments, "--host")?,
            "--remote-root" => {
                parsed.remote_root = argument_value(&mut arguments, "--remote-root")?
            }
            "--direct-root" => {
                parsed.direct_root = argument_value(&mut arguments, "--direct-root")?.into()
            }
            "--mount-root" => {
                parsed.mount_root = argument_value(&mut arguments, "--mount-root")?.into()
            }
            "--files" => parsed.files = argument_value(&mut arguments, "--files")?.parse()?,
            "--bytes" => parsed.bytes = argument_value(&mut arguments, "--bytes")?.parse()?,
            "--rounds" => parsed.rounds = argument_value(&mut arguments, "--rounds")?.parse()?,
            "--reuse-paths" => parsed.reuse_paths = true,
            "--api" => {
                parsed.apis = match argument_value(&mut arguments, "--api")?.as_str() {
                    "public" => &[Api::Public],
                    "backend" => &[Api::Backend],
                    "both" => &[Api::Public, Api::Backend],
                    _ => return Err("--api must be public, backend, or both".into()),
                };
            }
            "--help" | "-h" => {
                println!(
                    "Usage: small_files_benchmark [--host HOST] [--remote-root PATH] \
                     [--direct-root PATH] [--mount-root PATH] [--files N] \
                     [--bytes N] [--rounds N] [--reuse-paths] [--api public|backend|both]"
                );
                std::process::exit(0);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument: {argument}"),
                )
                .into());
            }
        }
    }
    if parsed.files == 0 || parsed.bytes == 0 || parsed.rounds == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--files, --bytes, and --rounds must be positive",
        )
        .into());
    }
    Ok(parsed)
}

fn remote_path(root: &str, relative: &str) -> String {
    let root = root.trim_matches('/');
    if root.is_empty() {
        format!("/{relative}")
    } else {
        format!("/{root}/{relative}")
    }
}

fn elapsed<T>(operation: impl FnOnce() -> T) -> (f64, T) {
    let started = Instant::now();
    let result = operation();
    (started.elapsed().as_secs_f64() * 1000.0, result)
}

fn measure_vnfs_write(fs: &mut BenchmarkFs, paths: &[String], payload: &[u8]) -> Result<Sample> {
    vfsi_nfs::compound::thread_compound_stats();
    vfsi_nfs::compound::rpc_stats();
    let (milliseconds, result) = elapsed(|| fs.write(paths, payload));
    result?;
    Ok(Sample {
        milliseconds,
        compounds: vfsi_nfs::compound::thread_compound_stats().0,
        rpcs: vfsi_nfs::compound::rpc_stats().0,
    })
}

fn measure_local_write(paths: &[PathBuf], payload: &[u8]) -> Result<f64> {
    let (milliseconds, result) = elapsed(|| {
        for path in paths {
            fs::write(path, payload)?;
        }
        Ok::<(), io::Error>(())
    });
    result?;
    Ok(milliseconds)
}

fn measure_vnfs_read(fs: &mut BenchmarkFs, paths: &[String], payload: &[u8]) -> Result<Sample> {
    vfsi_nfs::compound::thread_compound_stats();
    vfsi_nfs::compound::rpc_stats();
    let milliseconds = measure_read(|| fs.read(paths, payload.len()), paths.len(), payload)?;
    Ok(Sample {
        milliseconds,
        compounds: vfsi_nfs::compound::thread_compound_stats().0,
        rpcs: vfsi_nfs::compound::rpc_stats().0,
    })
}

fn measure_local_read(paths: &[PathBuf], payload: &[u8]) -> Result<f64> {
    measure_read(
        || Ok(paths.iter().map(fs::read).collect::<io::Result<_>>()?),
        paths.len(),
        payload,
    )
}

fn measure_read(
    read: impl FnOnce() -> Result<Vec<Vec<u8>>>,
    count: usize,
    payload: &[u8],
) -> Result<f64> {
    checked_elapsed(
        read,
        |results| {
            if results.len() != count || results.iter().any(|data| data != payload) {
                return Err(
                    io::Error::other("read returned unexpected data or result count").into(),
                );
            }
            Ok(())
        },
        Instant::now,
    )
    .map(|(milliseconds, _)| milliseconds)
}

// One timing boundary for both APIs and the kernel baseline. Injecting only
// the clock lets tests prove validation is excluded without sleeps or thresholds.
fn checked_elapsed<T>(
    operation: impl FnOnce() -> Result<T>,
    validate: impl FnOnce(&T) -> Result<()>,
    mut now: impl FnMut() -> Instant,
) -> Result<(f64, T)> {
    let start = now();
    let result = operation();
    let elapsed = now().duration_since(start).as_secs_f64() * 1000.0;
    let result = result?;
    validate(&result)?;
    Ok((elapsed, result))
}

// `usize::is_multiple_of` is newer than the crate's Rust 1.88 MSRV.
#[allow(clippy::manual_is_multiple_of)]
fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

fn sample_median(samples: &[Sample], select: impl Fn(&Sample) -> f64) -> f64 {
    median(samples.iter().map(select).collect())
}

fn prepare_paths(root: &Path, relative: &str) -> Result<()> {
    fs::create_dir_all(root.join(relative))?;
    Ok(())
}

fn run(args: &Args, api: Api, run_name: &str, direct_run: &Path, mount_run: &Path) -> Result<()> {
    let mut fs = BenchmarkFs::connect(&args.host, api)?;
    let payload: Vec<u8> = (0..args.bytes).map(|index| (index % 251) as u8).collect();
    let mut vnfs_write = Vec::with_capacity(args.rounds);
    let mut local_write = Vec::with_capacity(args.rounds);
    let mut vnfs_read = Vec::with_capacity(args.rounds);
    let mut local_read = Vec::with_capacity(args.rounds);

    prepare_paths(direct_run, "two-file-probe")?;
    let probe_paths = [
        remote_path(
            &args.remote_root,
            &format!("{run_name}/two-file-probe/file-1"),
        ),
        remote_path(
            &args.remote_root,
            &format!("{run_name}/two-file-probe/file-2"),
        ),
    ];
    vfsi_nfs::compound::thread_compound_stats();
    fs.write(&probe_paths, b"probe")?;
    let probe_compounds = vfsi_nfs::compound::thread_compound_stats().0;
    for name in ["file-1", "file-2"] {
        if fs::read(mount_run.join("two-file-probe").join(name))? != b"probe" {
            return Err("direct-root and mount-root must address the same export".into());
        }
    }

    for round in 0..args.rounds {
        let suffix = if args.reuse_paths {
            "warm".to_string()
        } else {
            round.to_string()
        };
        let vnfs_write_rel = format!("write-vnfs-{suffix}");
        let local_write_rel = format!("write-kernel-{suffix}");
        let vnfs_read_rel = format!("read-vnfs-{suffix}");
        let local_read_rel = format!("read-kernel-{suffix}");
        for relative in [
            &vnfs_write_rel,
            &local_write_rel,
            &vnfs_read_rel,
            &local_read_rel,
        ] {
            prepare_paths(direct_run, relative)?;
        }

        let vnfs_write_paths: Vec<String> = (0..args.files)
            .map(|index| {
                remote_path(
                    &args.remote_root,
                    &format!("{run_name}/{vnfs_write_rel}/file-{index:04}"),
                )
            })
            .collect();
        let local_write_paths: Vec<PathBuf> = (0..args.files)
            .map(|index| {
                mount_run
                    .join(&local_write_rel)
                    .join(format!("file-{index:04}"))
            })
            .collect();
        if args.reuse_paths && round == 0 {
            measure_vnfs_write(&mut fs, &vnfs_write_paths, &payload)?;
            measure_local_write(&local_write_paths, &payload)?;
        }
        if round % 2 == 0 {
            vnfs_write.push(measure_vnfs_write(&mut fs, &vnfs_write_paths, &payload)?);
            local_write.push(measure_local_write(&local_write_paths, &payload)?);
        } else {
            local_write.push(measure_local_write(&local_write_paths, &payload)?);
            vnfs_write.push(measure_vnfs_write(&mut fs, &vnfs_write_paths, &payload)?);
        }

        let vnfs_read_paths: Vec<String> = (0..args.files)
            .map(|index| {
                remote_path(
                    &args.remote_root,
                    &format!("{run_name}/{vnfs_read_rel}/file-{index:04}"),
                )
            })
            .collect();
        let local_read_paths: Vec<PathBuf> = (0..args.files)
            .map(|index| {
                mount_run
                    .join(&local_read_rel)
                    .join(format!("file-{index:04}"))
            })
            .collect();
        if !args.reuse_paths || round == 0 {
            for index in 0..args.files {
                fs::write(
                    direct_run
                        .join(&vnfs_read_rel)
                        .join(format!("file-{index:04}")),
                    &payload,
                )?;
                fs::write(
                    direct_run
                        .join(&local_read_rel)
                        .join(format!("file-{index:04}")),
                    &payload,
                )?;
            }
        }
        if args.reuse_paths && round == 0 {
            measure_vnfs_read(&mut fs, &vnfs_read_paths, &payload)?;
            measure_local_read(&local_read_paths, &payload)?;
        }
        if round % 2 == 0 {
            vnfs_read.push(measure_vnfs_read(&mut fs, &vnfs_read_paths, &payload)?);
            local_read.push(measure_local_read(&local_read_paths, &payload)?);
        } else {
            local_read.push(measure_local_read(&local_read_paths, &payload)?);
            vnfs_read.push(measure_vnfs_read(&mut fs, &vnfs_read_paths, &payload)?);
        }
    }

    let vnfs_write_ms = sample_median(&vnfs_write, |sample| sample.milliseconds);
    let local_write_ms = median(local_write);
    let vnfs_read_ms = sample_median(&vnfs_read, |sample| sample.milliseconds);
    let local_read_ms = median(local_read);
    println!(
        "{{\n  \"api\": \"{}\",\n  \"bytes_per_file\": {},\n  \"files\": {},\n  \"path_mode\": \"{}\",\n  \
         \"rounds\": {},\n  \"two_file_write_compounds\": {},\n  \"write\": {{\n    \
         \"vnfs_median_ms\": {:.6},\n    \"kernel_nfs_median_ms\": {:.6},\n    \
         \"speedup\": {:.6},\n    \"vnfs_median_compounds\": {:.1},\n    \
         \"vnfs_median_rpcs\": {:.1}\n  }},\n  \"read\": {{\n    \"vnfs_median_ms\": {:.6},\n    \
         \"kernel_nfs_median_ms\": {:.6},\n    \"speedup\": {:.6},\n    \
         \"vnfs_median_compounds\": {:.1},\n    \"vnfs_median_rpcs\": {:.1}\n  }}\n}}",
        api.name(),
        args.bytes,
        args.files,
        if args.reuse_paths { "warm" } else { "cold" },
        args.rounds,
        probe_compounds,
        vnfs_write_ms,
        local_write_ms,
        local_write_ms / vnfs_write_ms,
        sample_median(&vnfs_write, |sample| sample.compounds as f64),
        sample_median(&vnfs_write, |sample| sample.rpcs as f64),
        vnfs_read_ms,
        local_read_ms,
        local_read_ms / vnfs_read_ms,
        sample_median(&vnfs_read, |sample| sample.compounds as f64),
        sample_median(&vnfs_read, |sample| sample.rpcs as f64),
    );
    Ok(())
}

fn main() -> Result<()> {
    let args = parse_args(env::args().skip(1))?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for &api in args.apis {
        let run_name = format!(
            "vnfs-benchmark-{}-{nonce}-{}",
            std::process::id(),
            api.name()
        );
        let direct_run = args
            .direct_root
            .join(args.remote_root.trim_matches('/'))
            .join(&run_name);
        let mount_run = args
            .mount_root
            .join(args.remote_root.trim_matches('/'))
            .join(&run_name);
        fs::create_dir_all(&direct_run)?;
        let result = run(&args, api, &run_name, &direct_run, &mount_run);
        let cleanup = fs::remove_dir_all(&direct_run);
        result?;
        cleanup?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, time::Duration};

    #[test]
    fn read_validation_is_excluded_from_elapsed_time() {
        let base = Instant::now();
        let clock = Cell::new(0);
        let (milliseconds, value) = checked_elapsed(
            || {
                clock.set(3);
                Ok(42)
            },
            |value| {
                assert_eq!(*value, 42);
                clock.set(103);
                Ok(())
            },
            || base + Duration::from_millis(clock.get()),
        )
        .unwrap();
        assert_eq!(milliseconds, 3.0);
        assert_eq!(value, 42);
        assert_eq!(clock.get(), 103, "validation must still run");
    }

    #[test]
    fn read_measurements_reject_corruption_cardinality_and_io_errors() {
        for replies in [vec![b"bad".to_vec()], vec![], vec![b"good".to_vec(); 2]] {
            assert!(measure_read(|| Ok(replies), 1, b"good").is_err());
        }
        assert!(
            measure_read(
                || Err(io::Error::from_raw_os_error(libc::EIO).into()),
                1,
                b"good"
            )
            .is_err()
        );
        assert!(measure_read(|| Ok(vec![b"good".to_vec()]), 1, b"good").is_ok());
    }

    #[test]
    fn public_api_is_the_default_and_backend_measurements_are_explicit() {
        assert_eq!(parse_args([]).unwrap().apis, [Api::Public]);
        assert_eq!(
            parse_args(["--api", "both"].map(String::from))
                .unwrap()
                .apis,
            [Api::Public, Api::Backend]
        );
        assert_eq!(
            parse_args(["--api", "backend"].map(String::from))
                .unwrap()
                .apis,
            [Api::Backend]
        );
        assert!(parse_args(["--api", "unknown"].map(String::from)).is_err());
    }
}
