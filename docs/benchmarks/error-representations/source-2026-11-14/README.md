# Error representation experiment

This nonpublished standalone Cargo package compares:
- A minimal index-free VfError-like cause.
- Owned std::io::Error with small typed protocol/transport/context payloads.
- Arc<std::io::Error> as an explicit shared-cause alternative.

It does not modify production vnfs code or published error APIs.

See [the measured comparison](../README.md).

## Reproduce on the configured VM

Restore this source at `benchmarks/error-representations` in a disposable
checkout of baseline `70bea6a`. From that checkout, use the existing build
environment, including its `CARGO_TARGET_DIR` and profile. Build two binaries
from identical source:
1. cargo build --offline --release --manifest-path benchmarks/error-representations/Cargo.toml
2. Copy $CARGO_TARGET_DIR/release/vferror-comparison to a new owned timing path.
3. cargo build --offline --release --features allocation-stats --manifest-path benchmarks/error-representations/Cargo.toml
4. Copy that executable to a separate owned allocation path.
5. Run python benchmarks/error-representations/run.py --timing TIMING_BINARY
   --allocations ALLOCATION_BINARY --output NEW_DIRECTORY --pairs 9
   --seconds 0.1 --cpu AN_ALLOWED_CPU

The driver refuses an existing output directory and optimized Python mode, owns
its fixture, checks work/I/O counts and fixture hashes, records source/binary
hashes and actual build-compiler provenance, and removes only its own fixture.
Do not run competing builds or benchmarks during the timing phase. Timing samples
never come from the allocation-instrumented binary.

The five Rust tests require VFERROR_BENCH_FIXTURE pointing to an owned directory
with file-0 through file-63 containing 4,096 bytes each, plus an empty directory
named directory. Run cargo test --manifest-path
benchmarks/error-representations/Cargo.toml -- --test-threads=1.
Missing fixture/ring support fails this required test rather than silently skipping.

Known-payload reconstruction is not a Clone implementation for arbitrary
std::io::Error. The opaque-payload test enforces that limitation. No general
migration or new public error helper is implemented here.
