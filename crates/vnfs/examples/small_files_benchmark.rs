//! Compare vnfs vector I/O with scalar std::fs calls over a kernel NFS mount.

use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use vnfs::{NfsVecFs, ReadOp, VecFs, VfOffset, WriteOp};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

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

fn parse_args() -> Result<Args> {
    let mut parsed = Args::default();
    let mut arguments = env::args().skip(1);
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
            "--help" | "-h" => {
                println!(
                    "Usage: small_files_benchmark [--host HOST] [--remote-root PATH] \
                     [--direct-root PATH] [--mount-root PATH] [--files N] \
                     [--bytes N] [--rounds N] [--reuse-paths]"
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

fn write_ops(paths: &[String], payload: &[u8]) -> Vec<WriteOp> {
    paths
        .iter()
        .map(|path| {
            WriteOp::from_path(path, VfOffset::At(0), payload.to_vec())
                .with_creation()
                .with_truncate()
        })
        .collect()
}

fn read_ops(paths: &[String], length: usize) -> Vec<ReadOp> {
    paths
        .iter()
        .map(|path| ReadOp::from_path(path, VfOffset::At(0), length))
        .collect()
}

fn measure_vnfs_write(client: &mut NfsVecFs, operations: &[WriteOp]) -> Result<Sample> {
    vnfs::compound::thread_compound_stats();
    vnfs::compound::rpc_stats();
    let (milliseconds, result) = elapsed(|| client.writev(operations));
    result?;
    Ok(Sample {
        milliseconds,
        compounds: vnfs::compound::thread_compound_stats().0,
        rpcs: vnfs::compound::rpc_stats().0,
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

fn measure_vnfs_read(
    client: &mut NfsVecFs,
    operations: &[ReadOp],
    payload: &[u8],
) -> Result<Sample> {
    vnfs::compound::thread_compound_stats();
    vnfs::compound::rpc_stats();
    let (milliseconds, result) = elapsed(|| client.readv(operations));
    let results = result?;
    if results.len() != operations.len() || results.iter().any(|item| item.data != payload) {
        return Err(io::Error::other("vnfs returned unexpected data").into());
    }
    Ok(Sample {
        milliseconds,
        compounds: vnfs::compound::thread_compound_stats().0,
        rpcs: vnfs::compound::rpc_stats().0,
    })
}

fn measure_local_read(paths: &[PathBuf], payload: &[u8]) -> Result<f64> {
    let (milliseconds, result) = elapsed(|| {
        for path in paths {
            if fs::read(path)? != payload {
                return Err(io::Error::other("std::fs returned unexpected data"));
            }
        }
        Ok::<(), io::Error>(())
    });
    result?;
    Ok(milliseconds)
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
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

fn run(args: &Args, run_name: &str, direct_run: &Path, mount_run: &Path) -> Result<()> {
    let mut client = NfsVecFs::connect_minor(&args.host, 2)?;
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
    let probe_ops = write_ops(&probe_paths, b"probe");
    vnfs::compound::thread_compound_stats();
    client.writev(&probe_ops)?;
    let probe_compounds = vnfs::compound::thread_compound_stats().0;

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
        let writes = write_ops(&vnfs_write_paths, &payload);
        if args.reuse_paths && round == 0 {
            measure_vnfs_write(&mut client, &writes)?;
            measure_local_write(&local_write_paths, &payload)?;
        }
        if round % 2 == 0 {
            vnfs_write.push(measure_vnfs_write(&mut client, &writes)?);
            local_write.push(measure_local_write(&local_write_paths, &payload)?);
        } else {
            local_write.push(measure_local_write(&local_write_paths, &payload)?);
            vnfs_write.push(measure_vnfs_write(&mut client, &writes)?);
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
        let reads = read_ops(&vnfs_read_paths, payload.len());
        if args.reuse_paths && round == 0 {
            measure_vnfs_read(&mut client, &reads, &payload)?;
            measure_local_read(&local_read_paths, &payload)?;
        }
        if round % 2 == 0 {
            vnfs_read.push(measure_vnfs_read(&mut client, &reads, &payload)?);
            local_read.push(measure_local_read(&local_read_paths, &payload)?);
        } else {
            local_read.push(measure_local_read(&local_read_paths, &payload)?);
            vnfs_read.push(measure_vnfs_read(&mut client, &reads, &payload)?);
        }
    }

    let vnfs_write_ms = sample_median(&vnfs_write, |sample| sample.milliseconds);
    let local_write_ms = median(local_write);
    let vnfs_read_ms = sample_median(&vnfs_read, |sample| sample.milliseconds);
    let local_read_ms = median(local_read);
    println!(
        "{{\n  \"bytes_per_file\": {},\n  \"files\": {},\n  \"path_mode\": \"{}\",\n  \
         \"rounds\": {},\n  \"two_file_write_compounds\": {},\n  \"write\": {{\n    \
         \"vnfs_median_ms\": {:.6},\n    \"kernel_nfs_median_ms\": {:.6},\n    \
         \"speedup\": {:.6},\n    \"vnfs_median_compounds\": {:.1},\n    \
         \"vnfs_median_rpcs\": {:.1}\n  }},\n  \"read\": {{\n    \"vnfs_median_ms\": {:.6},\n    \
         \"kernel_nfs_median_ms\": {:.6},\n    \"speedup\": {:.6},\n    \
         \"vnfs_median_compounds\": {:.1},\n    \"vnfs_median_rpcs\": {:.1}\n  }}\n}}",
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
    let args = parse_args()?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let run_name = format!("vnfs-benchmark-{}-{nonce}", std::process::id());
    let direct_run = args
        .direct_root
        .join(args.remote_root.trim_matches('/'))
        .join(&run_name);
    let mount_run = args
        .mount_root
        .join(args.remote_root.trim_matches('/'))
        .join(&run_name);
    fs::create_dir_all(&direct_run)?;
    let result = run(&args, &run_name, &direct_run, &mount_run);
    let cleanup = fs::remove_dir_all(&direct_run);
    result?;
    cleanup?;
    Ok(())
}
