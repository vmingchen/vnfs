//! Measure bounded single-file NFS streaming across caller-selected chunk sizes.
//!
//! The callback only counts bytes; it does not retain file contents. This
//! keeps memory use bounded and measures the normal decode/callback path, not
//! a zero-copy experiment.

use std::error::Error;
use std::io;
use std::time::{Duration, Instant};

use vnfs::{Nfs, NfsReadPool, NfsReadPoolOptions, ReadStreamOptions};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug)]
struct Args {
    host: String,
    root: String,
    minor_version: Option<u32>,
    path: String,
    rounds: usize,
    warmups: usize,
    chunk_sizes: Vec<usize>,
    worker_counts: Vec<usize>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            root: "/".into(),
            minor_version: None,
            path: String::new(),
            rounds: 5,
            warmups: 1,
            chunk_sizes: vec![64 * 1024, 256 * 1024, 1024 * 1024, 4 * 1024 * 1024],
            worker_counts: vec![2, 4],
        }
    }
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{flag} needs a value")).into()
    })
}

fn parse() -> Result<Args> {
    let mut parsed = Args::default();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--host" => parsed.host = value(&mut args, "--host")?,
            "--root" => parsed.root = value(&mut args, "--root")?,
            "--minor-version" => {
                let version = value(&mut args, "--minor-version")?.parse::<u32>()?;
                if !matches!(version, 1 | 2) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--minor-version must be 1 or 2",
                    )
                    .into());
                }
                parsed.minor_version = Some(version);
            }
            "--path" => parsed.path = value(&mut args, "--path")?,
            "--rounds" => parsed.rounds = value(&mut args, "--rounds")?.parse()?,
            "--warmups" => parsed.warmups = value(&mut args, "--warmups")?.parse()?,
            "--chunk-sizes" => {
                parsed.chunk_sizes = value(&mut args, "--chunk-sizes")?
                    .split(',')
                    .map(str::parse)
                    .collect::<std::result::Result<_, _>>()?;
            }
            "--worker-counts" => {
                parsed.worker_counts = value(&mut args, "--worker-counts")?
                    .split(',')
                    .map(str::parse)
                    .collect::<std::result::Result<_, _>>()?;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: cargo run --release -p vnfs --example large_file_read_benchmark --features nfs -- --host HOST --root EXPORT_ROOT --path FILE [--minor-version 1|2] [--rounds N] [--warmups N] [--chunk-sizes BYTES,...] [--worker-counts N,...]"
                );
                std::process::exit(0);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown option: {flag}"),
                )
                .into());
            }
        }
    }
    if parsed.path.is_empty()
        || parsed.rounds == 0
        || parsed.chunk_sizes.is_empty()
        || parsed.chunk_sizes.contains(&0)
        || parsed.worker_counts.is_empty()
        || parsed.worker_counts.contains(&0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--path, --rounds, and non-zero --chunk-sizes are required",
        )
        .into());
    }
    Ok(parsed)
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn read_once(
    client: &vnfs::NfsClient,
    path: &str,
    chunk_size: usize,
) -> Result<(Duration, u64, u64)> {
    let mut bytes = 0u64;
    let mut chunks = 0u64;
    let started = Instant::now();
    client.read_stream_with_options(
        path,
        ReadStreamOptions::new().chunk_size(chunk_size),
        |_, data| {
            bytes += data.len() as u64;
            chunks += 1;
            Ok(true)
        },
    )?;
    Ok((started.elapsed(), bytes, chunks))
}

fn read_pipelined_once(pool: &mut NfsReadPool, path: &str) -> Result<(Duration, u64, u64)> {
    let mut bytes = 0u64;
    let mut chunks = 0u64;
    let started = Instant::now();
    pool.read_stream(path, |_, data| {
        bytes += data.len() as u64;
        chunks += 1;
        Ok(true)
    })?;
    Ok((started.elapsed(), bytes, chunks))
}

fn main() -> Result<()> {
    let args = parse()?;
    let client = Nfs::builder(args.host.clone())
        .root(args.root.clone())
        .minor_version(args.minor_version)
        .connect()?;

    println!("streaming {} without retaining contents", args.path);
    println!("requested_nfs_minor_version={:?}", args.minor_version);
    println!("mode,workers,chunk_bytes,bytes,setup_ms,median_ms,median_mib_per_sec,median_chunks");
    for &chunk_size in &args.chunk_sizes {
        for _ in 0..args.warmups {
            read_once(&client, &args.path, chunk_size)?;
        }
        let mut samples = Vec::with_capacity(args.rounds);
        for _ in 0..args.rounds {
            samples.push(read_once(&client, &args.path, chunk_size)?);
        }
        let bytes = samples[0].1;
        if samples.iter().any(|sample| sample.1 != bytes) {
            return Err(io::Error::other("file size changed during benchmark").into());
        }
        let elapsed = median(
            samples
                .iter()
                .map(|sample| sample.0.as_secs_f64())
                .collect(),
        );
        let chunks = median(samples.iter().map(|sample| sample.2 as f64).collect());
        let mib_per_sec = bytes as f64 / (1024.0 * 1024.0) / elapsed;
        println!(
            "sequential,1,{chunk_size},{bytes},0,{:.3},{mib_per_sec:.2},{chunks:.0}",
            elapsed * 1000.0
        );

        for &worker_count in &args.worker_counts {
            let setup_started = Instant::now();
            let mut pool = Nfs::builder(args.host.clone())
                .root(args.root.clone())
                .minor_version(args.minor_version)
                .connect_read_pool(
                    NfsReadPoolOptions::new()
                        .worker_count(worker_count)
                        .chunk_size(chunk_size)
                        .max_in_flight(worker_count * 2)
                        .max_buffered_bytes(16 * 1024 * 1024),
                )?;
            let setup_ms = setup_started.elapsed().as_secs_f64() * 1000.0;
            for _ in 0..args.warmups {
                read_pipelined_once(&mut pool, &args.path)?;
            }
            let mut samples = Vec::with_capacity(args.rounds);
            for _ in 0..args.rounds {
                samples.push(read_pipelined_once(&mut pool, &args.path)?);
            }
            if samples.iter().any(|sample| sample.1 != bytes) {
                return Err(io::Error::other("file size changed during pipeline benchmark").into());
            }
            let elapsed = median(
                samples
                    .iter()
                    .map(|sample| sample.0.as_secs_f64())
                    .collect(),
            );
            let chunks = median(samples.iter().map(|sample| sample.2 as f64).collect());
            let mib_per_sec = bytes as f64 / (1024.0 * 1024.0) / elapsed;
            println!(
                "pipeline,{worker_count},{chunk_size},{bytes},{setup_ms:.3},{:.3},{mib_per_sec:.2},{chunks:.0}",
                elapsed * 1000.0
            );
        }
    }
    Ok(())
}
