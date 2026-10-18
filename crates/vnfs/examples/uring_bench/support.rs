//! Benchmark-only configuration/reporting; deliberately outside the library API.
use super::{Case, Mode};
use serde_json::{Value, json};
use std::{
    error::Error,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};
use vfsi_uring::{Stats, Telemetry};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
pub const USAGE: &str = "uring_bench [rounds] [parent] [both|both-reverse|posix|uring] [all|warm|cold|write-shapes|CASE] [queue_depth] [max_batch_bytes] [ring|cached|adaptive] [--files N] [--small-bytes N] [--large-bytes N] [--chunk-bytes N] [--order sequential|random] [--seed N] [--max-case-bytes N] [--json NEW_PATH]";
const WARM: &[&str] = &[
    "small-read",
    "small-write",
    "large-read",
    "large-write",
    "directories",
];
const COLD: &[&str] = &["cold-small-read", "cold-large-read"];
const OWNED: &[&str] = &[
    "owned-read",
    "owned-large-read",
    "owned-cold-read",
    "owned-cold-large-read",
];
const SHAPES: &[&str] = &["single-large-write", "large-write", "many-files-write"];

#[derive(Debug)]
pub struct Config {
    pub rounds: usize,
    pub parent: PathBuf,
    pub depth: u32,
    pub window: usize,
    pub execution: String,
    pub files: usize,
    backend: String,
    phase: String,
    small: usize,
    large: usize,
    chunk: usize,
    random: bool,
    seed: u64,
    max_case_bytes: usize,
    output: Option<PathBuf>,
}

impl Config {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let args: Vec<_> = args.into_iter().collect();
        let split = args
            .iter()
            .position(|s| s.starts_with("--"))
            .unwrap_or(args.len());
        if split > 7 {
            return Err(USAGE.into());
        }
        let positional = &args[..split];
        let get = |i: usize, default: &str| {
            positional
                .get(i)
                .map(String::as_str)
                .unwrap_or(default)
                .to_owned()
        };
        let mut c = Self {
            rounds: get(0, "100").parse()?,
            parent: get(1, "target").into(),
            backend: get(2, "both"),
            phase: get(3, "all"),
            depth: get(4, "256").parse()?,
            window: get(5, "2097152").parse()?,
            execution: get(6, "ring"),
            files: 256,
            small: 4096,
            large: 16 * 1024 * 1024,
            chunk: 128 * 1024,
            random: false,
            seed: 1,
            max_case_bytes: 64 * 1024 * 1024,
            output: None,
        };
        let mut seen = std::collections::HashSet::new();
        let mut extra = args[split..].iter();
        while let Some(flag) = extra.next() {
            if !seen.insert(flag) {
                return Err(format!("duplicate option: {flag}").into());
            }
            let value = extra
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            match flag.as_str() {
                "--files" => c.files = value.parse()?,
                "--small-bytes" => c.small = value.parse()?,
                "--large-bytes" => c.large = value.parse()?,
                "--chunk-bytes" => c.chunk = value.parse()?,
                "--seed" => c.seed = value.parse()?,
                "--max-case-bytes" => c.max_case_bytes = value.parse()?,
                "--json" => c.output = Some(value.into()),
                "--order" => {
                    c.random = match value.as_str() {
                        "sequential" => false,
                        "random" => true,
                        _ => return Err("order must be sequential or random".into()),
                    }
                }
                _ => return Err(format!("unknown option: {flag}; {USAGE}").into()),
            }
        }
        if !(1..=1_000_000).contains(&c.rounds)
            || !(1..=65_536).contains(&c.files)
            || !(1..=4096).contains(&c.depth)
            || c.window == 0
            || c.max_case_bytes == 0
            || c.small == 0
            || c.large == 0
            || c.chunk == 0
            || !c.large.is_multiple_of(c.chunk)
        {
            return Err("invalid geometry: nonzero sizes, divisible large/chunk bytes, files <= 65536, rounds <= 1000000, depth <= 4096 required".into());
        }
        if !matches!(
            c.backend.as_str(),
            "both" | "both-reverse" | "posix" | "uring"
        ) || !matches!(c.execution.as_str(), "ring" | "cached" | "adaptive")
        {
            return Err(USAGE.into());
        }
        c.cases()?; // Reject oversized cases before creating fixtures.
        Ok(c)
    }

    pub fn backends(&self) -> Vec<&'static str> {
        match self.backend.as_str() {
            "both-reverse" => vec!["uring", "posix"],
            "posix" => vec!["posix"],
            "uring" => vec!["uring"],
            _ => vec!["posix", "uring"],
        }
    }

    pub fn cases(&self) -> Result<Vec<Case>> {
        let all: Vec<_> = WARM
            .iter()
            .chain(COLD)
            .chain(&["mixed-read"])
            .chain(OWNED)
            .chain(SHAPES.iter().filter(|s| **s != "large-write"))
            .copied()
            .collect();
        let selected = match self.phase.as_str() {
            "all" => all,
            "warm" => WARM.to_vec(),
            "cold" => COLD.to_vec(),
            "write-shapes" => SHAPES.to_vec(),
            name => vec![
                all.into_iter()
                    .find(|s| *s == name)
                    .ok_or_else(|| format!("unknown case: {name}"))?,
            ],
        };
        selected.into_iter().map(|name| {
            let (size, count, independent) = match name {
                "directories" => (0, 16, true),
                "single-large-write" => (self.large, 1, false),
                "many-files-write" => {
                    if !self.large.is_multiple_of(self.files) { return Err("write-shapes requires large bytes divisible by files".into()); }
                    (self.large / self.files, self.files, true)
                }
                name if name.contains("large") => (self.chunk, self.large / self.chunk, false),
                _ => (self.small, self.files, true),
            };
            let total = size.checked_mul(count).ok_or("case byte count overflow")?;
            if total > self.max_case_bytes || count > 1_048_576 {
                return Err("case exceeds --max-case-bytes or 1048576 requests; increase budget explicitly or reduce geometry".into());
            }
            let mut offsets: Vec<_> = (0..count).map(|i| if independent { 0 } else { (i * size) as u64 }).collect();
            if self.random && !independent { shuffle(&mut offsets, self.seed); }
            let paths = if name == "directories" { (0..16).map(|i| format!("/dir-{i}")).collect() }
                else if independent { (0..count).map(|i| format!("/file-{i}")).collect() }
                else { vec!["/large".into()] };
            let mode = if name.ends_with("write") { Mode::Write }
                else if name.starts_with("owned-") { Mode::OwnedRead(name.contains("cold")) }
                else if name == "mixed-read" { Mode::MixedRead }
                else if name.contains("cold") { Mode::ColdRead }
                else { Mode::Read };
            Ok(Case { name, paths, offsets, size, mode })
        }).collect()
    }
}

fn shuffle(values: &mut [u64], seed: u64) {
    let mut state = seed;
    for i in (1..values.len()).rev() {
        // SplitMix64: deterministic permutations without a benchmark dependency.
        state = state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^= z >> 31;
        values.swap(i, (z % (i as u64 + 1)) as usize);
    }
}

fn descriptor_budget(soft: libc::rlim_t, open: usize, files: usize) -> Result<()> {
    // Backend handles plus independent validation handles, and room for the
    // root, ring, report and directory machinery. Do not change process limits.
    let needed = open
        .saturating_add(files.saturating_mul(2))
        .saturating_add(16);
    if needed as u128 > soft as u128 {
        return Err(format!("benchmark requires approximately {needed} descriptor slots, but RLIMIT_NOFILE is {soft}; raise the shell limit with ulimit -n (within its hard limit), or reduce --files").into());
    }
    Ok(())
}

pub fn check_fd_limit(cases: &[Case]) -> Result<()> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: limit is writable storage for getrlimit's result.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let open = std::fs::read_dir("/proc/self/fd")?.count();
    descriptor_budget(
        limit.rlim_cur,
        open,
        cases.iter().map(|case| case.paths.len()).max().unwrap_or(0),
    )
}

pub fn cpu_seconds() -> Result<f64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time points to writable storage; process CPU clock takes no other pointers.
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(time.tv_sec as f64 + time.tv_nsec as f64 * 1e-9)
}

fn percentile(sorted: &[u64], percent: usize) -> u64 {
    sorted[(sorted.len() * percent).div_ceil(100).max(1) - 1]
}

fn counters(before: Stats, after: Stats) -> Value {
    let mut fields = serde_json::Map::new();
    macro_rules! delta { ($($field:ident),* $(,)?) => { $(fields.insert(stringify!($field).into(), json!(after.$field - before.$field));)* }; }
    delta!(
        syscall_reads,
        syscall_writes,
        cache_probes,
        cache_hits,
        cache_misses,
        waves,
        submissions,
        completions,
        enters,
        scratch_growths,
        copied_read_bytes
    );
    // High-water marks cannot be subtracted. Fresh executors isolate each case.
    fields.insert(
        "case_peak_bytes_including_warmup".into(),
        json!(after.peak_bytes),
    );
    fields.insert(
        "case_scratch_growths_including_warmup".into(),
        json!(after.scratch_growths),
    );
    Value::Object(fields)
}

pub struct Report {
    pub telemetry: Option<Telemetry>,
    records: Vec<Value>,
    environment: Value,
    file: Option<File>,
}

impl Report {
    pub fn new(config: &Config, root: &Path) -> Result<Self> {
        // Fail before running if the result path already exists; never clobber a run.
        let file = config
            .output
            .as_ref()
            .map(|p| File::options().write(true).create_new(true).open(p))
            .transpose()?;
        let command = |name: &str, args: &[&str]| -> Option<String> {
            let output = Command::new(name).args(args).output().ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        };
        let manifest = env!("CARGO_MANIFEST_DIR");
        let executable = std::env::current_exe()?;
        let environment = json!({
            "unix_time_seconds": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),
            "kernel": command("uname", &["-srvm"]), "architecture": std::env::consts::ARCH,
            "available_parallelism": std::thread::available_parallelism()?.get(),
            "cpu": command("lscpu", &["-J"]).and_then(|s| serde_json::from_str::<Value>(&s).ok()),
            "memory": std::fs::read_to_string("/proc/meminfo").ok(),
            "process_limits": std::fs::read_to_string("/proc/self/limits").ok(),
            "debug_assertions": cfg!(debug_assertions),
            "filesystem": command("findmnt", &["-J", "-T", root.to_str().ok_or("non-UTF8 root")?, "-o", "TARGET,SOURCE,FSTYPE,OPTIONS"]),
            "source_revision_at_run": command("git", &["-C", manifest, "rev-parse", "HEAD"]),
            "source_status_at_run": command("git", &["-C", manifest, "status", "--porcelain"]),
            "executable": executable, "executable_sha256": command("sha256sum", &[executable.to_str().ok_or("non-UTF8 executable")?]),
            "rustc_at_run": command("rustc", &["--version"]), "scratch_parent": std::fs::canonicalize(&config.parent)?,
        });
        Ok(Self {
            telemetry: None,
            records: Vec::new(),
            environment,
            file,
        })
    }

    pub fn snapshot(&self) -> Stats {
        self.telemetry
            .as_ref()
            .map(Telemetry::snapshot)
            .unwrap_or_default()
    }

    pub fn record(
        &mut self,
        backend: &str,
        case: &Case,
        timing: (Vec<u64>, f64),
        stats: (Stats, Stats),
    ) {
        let (samples, cpu) = timing;
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        let p50 = percentile(&sorted, 50);
        let bytes = case.size * case.offsets.len();
        let throughput = bytes as f64 * 1e9 / (1024.0 * 1024.0 * p50.max(1) as f64);
        let operations = case.offsets.len();
        println!(
            "{backend}/{}: p50_us={:.1} p95_us={:.1} p99_us={:.1} MiB_s={throughput:.1}",
            case.name,
            p50 as f64 / 1000.0,
            percentile(&sorted, 95) as f64 / 1000.0,
            percentile(&sorted, 99) as f64 / 1000.0
        );
        let telemetry = self.telemetry.as_ref().map(|_| {
            assert_eq!(
                stats.1.submissions - stats.0.submissions,
                stats.1.completions - stats.0.completions
            );
            counters(stats.0, stats.1)
        });
        if let Some(value) = &telemetry {
            println!("  measured telemetry: {value}");
        }
        self.records.push(json!({"case": case.name, "backend": backend, "samples_ns": samples,
            "rounds": sorted.len(), "warmups": 5, "p50_ns": p50,
            "p95_ns": percentile(&sorted, 95), "p99_ns": percentile(&sorted, 99),
            "tail_estimates_low_sample_count": sorted.len() < 100,
            "mib_per_second_at_p50": throughput, "operations_per_second_at_p50": operations as f64 * 1e9 / p50.max(1) as f64,
            "process_cpu_seconds": cpu, "bytes_per_call": bytes, "requests_per_call": case.offsets.len(),
            "request_bytes": case.size, "files": case.paths.len(),
            "directory_entries_per_call": if case.name == "directories" { Some(256) } else { None },
            "allocation": if case.name == "directories" { "listing" } else if matches!(case.mode, Mode::OwnedRead(_)) { "owned" } else { "reused-caller-buffers" },
            "cache": match case.mode { Mode::MixedRead => "first-file-warm-rest-advisory-evicted", Mode::ColdRead | Mode::OwnedRead(true) => "advisory-evicted", _ => "warm" },
            "telemetry": telemetry}));
    }

    pub fn finish(self, config: &Config) -> Result<()> {
        if let Some(mut file) = self.file {
            serde_json::to_writer_pretty(
                &mut file,
                &json!({
                    "schema_version": 1, "environment": self.environment,
                    "config": {"rounds": config.rounds, "files": config.files, "small_bytes": config.small,
                        "large_bytes": config.large, "chunk_bytes": config.chunk, "queue_depth": config.depth,
                        "max_batch_bytes": config.window, "max_case_bytes": config.max_case_bytes,
                        "order": if config.random { "random" } else { "sequential" }, "seed": config.seed,
                        "execution_mode": config.execution, "backend_order": config.backends(), "phase": config.phase},
                    "notes": ["buffered writes exclude fsync", "eviction is advisory, not cold physical media", "opens/closes, setup and content verification excluded", "runtime source/compiler metadata does not prove binary build provenance", "telemetry deltas exclude warmups; per-case peaks include warmups", "order controls single-file ranges; small-file cases have one request per file", "max-case-bytes bounds payload; validation/alternate buffers can use approximately three times that storage"],
                    "cases": self.records,
                }),
            )?;
            writeln!(file)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(args: &[&str]) -> Result<Config> {
        Config::parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn complete_suite_has_every_case_once_and_selectors_preserve_defaults() {
        let c = config(&[]).unwrap();
        let cases = c.cases().unwrap();
        let names: Vec<_> = cases.iter().map(|c| c.name).collect();
        assert_eq!(names.len(), 14);
        assert_eq!(
            names.iter().collect::<std::collections::HashSet<_>>().len(),
            names.len()
        );
        for phase in ["warm", "cold", "write-shapes"]
            .into_iter()
            .chain(names.iter().copied())
        {
            assert!(
                config(&["1", "target", "both", phase])
                    .unwrap()
                    .cases()
                    .unwrap()
                    .iter()
                    .all(|c| names.contains(&c.name))
            );
        }
        assert_eq!(c.backends(), ["posix", "uring"]);
        assert_eq!(
            config(&["1", "target", "both-reverse"]).unwrap().backends(),
            ["uring", "posix"]
        );
        assert_eq!(
            config(&["1", "target", "posix"]).unwrap().backends(),
            ["posix"]
        );
        assert_eq!(cases[0].size * cases[0].offsets.len(), 256 * 4096);
    }

    #[test]
    fn bounded_geometry_and_equal_payload_shapes() {
        let c = config(&[
            "1",
            "target",
            "both",
            "write-shapes",
            "--files",
            "16",
            "--large-bytes",
            "65536",
            "--chunk-bytes",
            "4096",
        ])
        .unwrap();
        for case in c.cases().unwrap() {
            assert_eq!(case.size * case.offsets.len(), 65536);
        }
        for args in [
            vec!["--files", "0"],
            vec!["--files", "65537"],
            vec!["--small-bytes", "0"],
            vec!["--chunk-bytes", "3"],
            vec!["--max-case-bytes", "1024"],
            vec!["--small-bytes", "18446744073709551615"],
            vec!["--files", "16", "--files", "32"],
            vec!["--order", "invalid"],
            vec!["--json"],
            vec!["--unknown", "1"],
            vec!["1", "target", "both", "nonsense"],
        ] {
            assert!(config(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn random_offsets_are_a_repeatable_nonoverlapping_permutation() {
        let c = config(&[
            "1",
            "target",
            "both",
            "large-write",
            "--order",
            "random",
            "--seed",
            "42",
        ])
        .unwrap();
        let mut first = c.cases().unwrap().remove(0).offsets;
        assert_eq!(first, c.cases().unwrap().remove(0).offsets);
        let sequential = config(&["1", "target", "both", "large-write"])
            .unwrap()
            .cases()
            .unwrap()
            .remove(0)
            .offsets;
        assert_ne!(first, sequential);
        first.sort_unstable();
        assert_eq!(first, sequential);
    }

    #[test]
    fn sample_percentiles_and_counter_deltas_are_not_lifetime_totals() {
        assert_eq!(percentile(&[10, 20, 30, 40], 50), 20);
        assert_eq!(percentile(&[10, 20, 30, 40], 99), 40);
        assert_eq!(percentile(&[10], 95), 10);
        let before = Stats {
            waves: 5,
            submissions: 10,
            peak_bytes: 4096,
            ..Stats::default()
        };
        let after = Stats {
            waves: 8,
            submissions: 16,
            peak_bytes: 4096,
            ..Stats::default()
        };
        let delta = counters(before, after);
        assert_eq!(delta["waves"], 3);
        assert_eq!(delta["submissions"], 6);
        assert_eq!(delta["case_peak_bytes_including_warmup"], 4096);
    }

    #[test]
    fn descriptor_preflight_accounts_for_both_backend_and_validation_handles() {
        assert!(descriptor_budget(1024, 4, 256).is_ok());
        let message = descriptor_budget(1024, 4, 512).unwrap_err().to_string();
        assert!(message.contains("1044 descriptor slots"));
        assert!(message.contains("ulimit -n"));
        assert!(descriptor_budget(16384, 4, 4096).is_ok());
    }

    #[test]
    fn json_round_trip_preserves_samples_and_never_overwrites_results() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("report.json");
        let c = config(&[
            "1",
            temp.path().to_str().unwrap(),
            "posix",
            "small-read",
            "--json",
            path.to_str().unwrap(),
        ])
        .unwrap();
        let case = c.cases().unwrap().remove(0);
        let mut report = Report::new(&c, temp.path()).unwrap();
        assert!(Report::new(&c, temp.path()).is_err());
        report.record(
            "posix",
            &case,
            (vec![30, 10, 20], 0.25),
            (Stats::default(), Stats::default()),
        );
        report.finish(&c).unwrap();
        let value: Value = serde_json::from_reader(File::open(path).unwrap()).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["cases"][0]["samples_ns"], json!([30, 10, 20]));
        assert_eq!(value["cases"][0]["p50_ns"], 20);
        assert_eq!(value["cases"][0]["process_cpu_seconds"], 0.25);
        assert!(value["cases"][0]["telemetry"].is_null());
        assert_eq!(value["config"]["backend_order"], json!(["posix"]));
    }
}
