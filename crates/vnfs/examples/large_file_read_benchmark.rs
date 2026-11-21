//! Measure bounded single-file NFS streaming across caller-selected chunk sizes.
//!
//! The callback only counts bytes; it does not retain file contents. This
//! keeps memory use bounded and measures the normal decode/callback path, not
//! a zero-copy experiment.

use std::error::Error;
use std::io;
use std::time::{Duration, Instant};

use vnfs::files::{StreamOptions, VfsiExt};
use vnfs::nfs::{Nfs, NfsReadPool, NfsReadPoolOptions};

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

fn parse(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
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
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--path, --rounds, and non-zero --chunk-sizes are required",
        )
        .into());
    }
    let max_workers = usize::MAX / 2;
    if parsed.worker_counts.is_empty()
        || parsed
            .worker_counts
            .iter()
            .any(|count| !(1..=max_workers).contains(count))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--worker-counts must be between 1 and {max_workers}"),
        )
        .into());
    }
    Ok(parsed)
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

fn read_once(
    client: &vnfs::nfs::NfsClient,
    path: &str,
    chunk_size: usize,
) -> Result<(Duration, u64, u64)> {
    let mut bytes = 0u64;
    let mut chunks = 0u64;
    let started = Instant::now();
    client.read_stream_with_options(
        path,
        StreamOptions::new().with_chunk_size(std::num::NonZeroUsize::new(chunk_size).unwrap()),
        |_, data| {
            bytes += data.len() as u64;
            chunks += 1;
            Ok(std::ops::ControlFlow::Continue(()))
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
        Ok(std::ops::ControlFlow::Continue(()))
    })?;
    Ok((started.elapsed(), bytes, chunks))
}

fn main() -> Result<()> {
    let args = parse(std::env::args().skip(1))?;
    let client = Nfs::builder(args.host.clone())
        .root(args.root.clone())
        .version(vnfs::nfs::NfsVersion::try_from(args.minor_version)?)
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
            "sequential,1,{chunk_size},{bytes},0,{:.3},{mib_per_sec:.2},{chunks}",
            elapsed * 1000.0
        );

        for &worker_count in &args.worker_counts {
            let setup_started = Instant::now();
            let mut pool = Nfs::builder(args.host.clone())
                .root(args.root.clone())
                .version(vnfs::nfs::NfsVersion::try_from(args.minor_version)?)
                .connect_read_pool(
                    NfsReadPoolOptions::new()
                        .with_worker_count(
                            std::num::NonZeroUsize::new(worker_count)
                                .expect("nonzero tuning value"),
                        )
                        .with_chunk_size(
                            std::num::NonZeroUsize::new(chunk_size).expect("nonzero tuning value"),
                        )
                        .with_max_in_flight(
                            std::num::NonZeroUsize::new(worker_count * 2)
                                .expect("nonzero tuning value"),
                        )
                        .with_max_buffered_bytes(16 * 1024 * 1024),
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
                "pipeline,{worker_count},{chunk_size},{bytes},{setup_ms:.3},{:.3},{mib_per_sec:.2},{chunks}",
                elapsed * 1000.0
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_uses_the_midpoint_for_even_samples() {
        assert_eq!(median(vec![8.0, 1.0, 5.0, 2.0]), 3.5);
        assert_eq!(median(vec![8.0, 1.0, 5.0]), 5.0);
    }

    #[test]
    fn rejects_worker_counts_that_cannot_form_a_read_window() {
        let parse_worker_count = |count: usize| {
            parse([
                "--path".to_owned(),
                "/file".to_owned(),
                "--worker-counts".to_owned(),
                count.to_string(),
            ])
        };
        for count in [0, usize::MAX / 2 + 1, usize::MAX] {
            let error = parse_worker_count(count).unwrap_err();
            assert!(error.to_string().contains("--worker-counts"));
        }
        let largest = usize::MAX / 2;
        assert_eq!(
            parse_worker_count(largest).unwrap().worker_counts,
            [largest]
        );
    }
}
