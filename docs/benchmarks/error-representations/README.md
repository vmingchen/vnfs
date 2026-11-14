# std::io::Error versus minimal index-free VfError

Date: 2026-11-14. Baseline: 70bea6a. Linux aarch64,
compiled with Rust 1.93.1 / LLVM 21.1.8. No production APIs were changed.

**Recommendation: use owned std::io::Error as the public cause type.**
Keep OpError for certainty/progress and IndexedError for lossy-helper attribution.
Given the preference for a familiar public API, the measured healthy-read parity
and smaller small-result slots justify this choice. The custom type is better
for allocation-free native protocol errors and cheap native cloning; that cost
must be acknowledged rather than hidden.

## Experiment and reproducibility

- [Raw evidence](results-2026-11-14.json) includes layouts, 69 separate allocation
  controls and 621 timed samples: 23 scenarios, three candidates, nine randomized
  paired triplets per scenario. Samples run pinned to CPU 2.
- Each measurement has ten unmeasured warm-up iterations. Pilot runs choose one
  shared iteration count per scenario targeting about 0.1 seconds for the slowest
  candidate. Measured byte/input counts and I/O counts are asserted equal.
- Timing and allocation binaries are separate release builds of the same source.
  Allocation tracking is compiled out of the timing binary. Source/binary hashes,
  actual build-compiler identity, Cargo metadata and fixture hashes are recorded.
- Fixtures are 64 files of 4,096 bytes on /tmp's tmpfs, plus a directory used for
  a real EISDIR error. Fixture contents were verified unchanged.
- [Frozen measured source and driver](source-2026-11-14/README.md) reuse
  vfsi-core's actual ReadResult, Attrs, FailureState and IoProgress. Both candidates
  use the same current portable native-status categorization table.
- Bootstrap intervals resample nine paired ratios (10,000 resamples). These are
  single-host microbenchmarks, not universal layout or throughput guarantees.

The candidates are:
1. Minimal index-free cause: native/client/transport detail plus optional boxed
   context, matching the proposed VfError shape.
2. Owned io::Error: OS errors use from_raw_os_error; simple categories use
   ErrorKind.into(); protocol statuses use small typed payloads. A rich context
   payload stores its context inline within the payload allocation, avoiding an
   unnecessary second context box.
3. Arc<io::Error> cause: a cloning/fan-out alternative. It adds an Arc allocation
   for each independently created cause. It is not a free Clone implementation
   for owned io::Error or a drop-in replacement for consuming-cause methods.

## Type sizes

Bytes on this measured target; not stable Rust or C ABI promises.

| Type | Current indexed VfError design | Minimal index-free | Owned io::Error |
| --- | ---: | ---: | ---: |
| Cause | 32 | 16 | 8 |
| Operation error | 48 | 32 | 24 |
| Void-result slot | 48 | 32 | 24 |
| Read-result slot | 48 | 40 | 40 |
| Attribute-result slot | 144 | 144 | 144 |
| Indexed wrapper | — | 32 | 24 |

ReadResult itself is 40 bytes and Attrs is 144 bytes. Their sizes limit how much
shrinking the error side can shrink the result slot. Removing the current index
benefits both designs; it is not a special std::io::Error speedup.

For 64 void-result slots, owned io::Error saves 512 inline bytes relative to the
minimal cause (1,536 versus 2,048). For read and attribute slots, the two proposed
designs use the same inline storage. Heap error payloads are additional storage:
an NFS-error OpError is 24 inline bytes plus 32 heap bytes with owned io::Error,
versus 32 inline bytes and no heap payload with the minimal cause.

## Allocations

Each entry is allocation calls / requested heap bytes per newly created error.
Allocation and deallocation are included in the measurement. Context/source
setup shared by both candidates is outside the measured interval; cloning the
path, context and source reference for each error is included. Static messages
do not allocate string contents.

| Error | Minimal index-free | Owned io::Error |
| --- | ---: | ---: |
| Bare ErrorKind | 0 / 0 | 0 / 0 |
| OS errno | 0 / 0 | 0 / 0 |
| NFS status | 0 / 0 | 2 / 32 |
| SMB status | 0 / 0 | 2 / 32 |
| Transport with static message | 1 / 72 | 2 / 56 |
| Native error + context/path/source | 2 / 87 | 3 / 119 |

On this Rust version, a boxed typed native payload plus io::Error's custom-error
storage accounts for two allocations totaling 32 bytes for an NFS/SMB error.
This is measured implementation behavior, not a promised standard-library layout.
[The standard documentation](https://doc.rust-lang.org/std/io/struct.Error.html#method.new)
guarantees that Error::new allocates, while the no-payload ErrorKind conversion
does not require that allocation.

The shared candidate adds one 24-byte allocation when initially wrapping each
cause in Arc. Making every error shared would therefore impose unnecessary
costs on independent failures. Do not infer total memory usage from cause size
alone.

## Cloning and fan-out

io::Error has no Clone implementation. Its opaque payload might not be clonable.
The owned-io column below measures reconstruction of this experiment's **known**
payloads, preserving kind, native namespace/code, path and original source.
The tests explicitly reject using that helper to clone an arbitrary payload.
It must not become a generic lossy cloning workaround.

Median nanoseconds per cloned/reconstructed existing cause:

| Existing cause | Minimal Clone | Owned io: known-payload reconstruction | Arc clone |
| --- | ---: | ---: | ---: |
| OS errno | 5.75 | 4.47 | 10.31 |
| NFS status | 5.22 | 19.05 | 10.30 |
| Contextual error | 41.20 | 50.69 | 10.29 |

Fan-out of one already-existing NFS cause into 64 failed void-result slots:
- Minimal: 0.335 microseconds; 1 vector allocation, 2,048 bytes.
- Owned io reconstruction: 3.047 microseconds; 129 allocations, 3,584 bytes.
- Shared Arc cause: 0.627 microseconds; 1 vector allocation, 1,536 bytes.

Those fan-out counts exclude construction of the original cause; its creation
cost is reported separately. Arc preserves the original arbitrary payload.
Creating 64 separate Arc-wrapped errors instead of sharing one error does not
obtain this fan-out benefit. Arc clone timings are single-threaded; they do not
measure cross-thread reference-count contention.

For contextual-cause fan-out, the shared candidate also avoids repeated context
and path cloning. Native cloning in the minimal type remains especially cheap
because bare native status is inline and Copy.

## Real read controls

Median microseconds per 64-input cohort. Ratios/intervals are owned-io versus
minimal; below one favors owned io. These are kernel descriptor read controls,
not calls through a fully migrated Vfsi backend.

| Workload | Minimal | Owned io | Paired ratio (95% interval) |
| --- | ---: | ---: | --- |
| POSIX: 64 successful 4 KiB reads | 21.712 | 21.566 | 0.994 (0.981–0.999) |
| POSIX: one EISDIR, 63 successful reads | 21.463 | 21.471 | 1.000 (0.992–1.008) |
| io_uring: 64 successful 4 KiB reads | 26.449 | 26.293 | 0.999 (0.992–1.003) |
| io_uring: one EISDIR, 63 successful reads | 27.952 | 27.932 | 1.002 (0.989–1.005) |

The healthy controls are near parity, within about one percent in these samples.
No general speedup claim follows. Both representations retain exactly 64 POSIX
pread calls per measured cohort or 64 SQEs/64 CQEs in one io_uring wave.
The mixed controls have exactly one error and 63 successes. Their allocation
counts are the same for owned OS-errno errors.

Buffers, handles and ring construction are outside the measurement. Result
assembly and destruction are inside it. The reusable storage is bounded. Fatal
ring-error cleanup retains potentially in-flight buffers/descriptors rather than
freeing borrowed storage. This is not a full backend scheduler comparison.

## Failure-heavy completion assembly

Synthetic protocol completions isolate representation overhead; no NFS or SMB
server was contacted for these timings.

For 64 independently failed NFS inputs:
- Minimal: 1.562 microseconds, 2 common vector allocations / 2,816 bytes.
- Owned io: 4.157 microseconds, 130 allocations / 4,864 bytes.
- Arc-wrapped independent io: 5.704 microseconds, 194 allocations / 6,400 bytes.

Owned io is about 2.67 times
slower here (about 2.595 extra microseconds per 64 failed inputs).
It cannot honestly be described as universally faster or allocation-free.

With only one synthetic NFS failure among 64 inputs, timings were near parity,
but owned io still added the native payload's two allocations. These synthetic
measurements do not establish end-to-end NFS/SMB latency during an error storm.

## Proper error handling and recommended API

Use ErrorKind for common portable decisions and io::Error for the actual cause:

```rust
pub type VecResult<T> = std::io::Result<Vec<OpResult<T>>>;

// Conceptual fields; execution knowledge stays independent of the cause.
struct OpError {
    cause: std::io::Error,
    state: FailureState,
    progress: Option<Box<IoProgress>>,
}
struct IndexedError {
    index: Option<usize>,
    cause: std::io::Error,
}
```

Implementation requirements:
- Preserve OS errno directly. Preserve exact NFS/SMB namespace and status in
  private typed payloads, not strings and not fabricated OS errno.
- Preserve transport provenance and path/source diagnostics in typed payloads.
  Adapters need a small typed extraction facility; hide construction/mapping
  machinery rather than replacing VfError with another public error wrapper.
- Keep FailureState and confirmed progress; neither ErrorKind nor an index tells
  whether a mutation ran, is uncertain, or is safe to retry.
- Do not require Clone/Eq on all public errors. Move owned errors when possible.
  A common infrastructure failure affecting several slots needs explicit
  ownership/fan-out handling, especially for arbitrary opaque source payloads.
- Share an original cause where justified. Do not silently stringify it, discard
  its status, or clone only ErrorKind. The direct Arc candidate demonstrates a
  possible internal shared-cause representation, not a mandate to wrap every
  independent error in Arc. A hybrid or consuming-conversion policy must be
  measured/validated when actually integrated.
- Keep attribution in slots or IndexedError; never add it back inside the cause.

The minimal custom type wins if allocation-free protocol-error throughput or
generic cheap cloning of bare native errors is the primary product requirement.
For the stated preference, owned io::Error wins the public API decision: familiar
constructors/category/OS-error inspection and standard interoperability outweigh
the measured per-protocol-error boxing cost. Its richer native-detail handling
still needs private code; removing a public custom error does not erase backend
semantics.

## Verification and limits

Five correctness tests passed, including actual POSIX/io_uring reads, exact
native status domains/categories, contextual path/source preservation, arbitrary
payload sharing, and certainty/progress/attribution separation.
[Unit log](tests.log) and [strict all-target/all-feature Clippy log](clippy.log)
are retained. No required ring test was ignored or fixture-skipped.

This experiment does not migrate production APIs, certify a C/Python binding
migration, measure live NFS/SMB error throughput, or establish performance on
other architectures/allocators. The [migration plan](../../vferror-simplification.md) adopts owned std::io::Error.
The frozen source uses vfsi-core from baseline 70bea6a; reproduce the historical
experiment by placing that source at benchmarks/error-representations in a
disposable checkout of that baseline. Its hashes remain separate from any later
benchmark adaptation to the new API.
