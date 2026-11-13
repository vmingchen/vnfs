# I/O outcome conformance

The production ABI remains v3. No test hooks or new library entry points are
added. The standalone C consumer in bindings/c/tests/io-outcomes.c links only
libdl and uses the packaged vfsi_dynamic.h. It resolves both optional I/O
functions before choosing a path. A missing function selects legacy I/O or
POSIX before dispatch. Errors from an extended call never trigger fallback.

## Shared real-server scenarios

ci/nfs_outcome_fixture.py supplies one deterministic NFS wire fixture to the
public Vfsi Rust example, standalone C consumer, and native Python consumer.
Each adapter reports its actual per-input states and confirmed bytes; the
runner compares them with the same SCENARIO_OUTCOMES. All rows run on NFSv4.1
and NFSv4.2. Consumers independently read back server effects through a fresh
direct connection and remove only their uniquely named file.

| Scenario / operation | Fault dispatch point | First input | Later valid input | Confirmed bytes | Retry policy |
|---|---|---|---|---|---|
| uncached, WRITE | Real WRITE executed; reply replaced with RETRY_UNCACHED_REP | Unknown, native NFS 10068 | none | 0 | No replay |
| lost, WRITE | Real WRITE executed; reply withheld and connection closed | Unknown, transport provenance | none | 0 | No replay |
| short, WRITE | First request shortened on wire to four bytes | Success | none | 16 after completing suffix | Exactly one suffix at offset 4, length 12 |
| prefix, WRITE | Four bytes acknowledged, next WRITE replaced by a zero-byte server WRITE and synthetic NOSPC | Failed, native NFS 28 | none | 4 | No retry after definite failure |
| later, WRITE | First wave of a 16 MiB write executes; reply lost | Unknown, transport provenance | NotAttempted, same cause | 0 for both | No suffix or later wave |
| uncached-later, WRITE | First wave executes; reply replaced with uncached status | Unknown, native NFS 10068 | Independent positional write succeeds | 0 / 1 | No uncertain suffix; independent work remains eligible |

Every row also includes an invalid/closed descriptor whose state is
NotAttempted, count zero, with no fabricated progress. Healthy tests cover mixed
success, missing and retired descriptors, read-only failures, short reads, EOF,
empty writes, append offsets, rejection before side effects, and descriptor
retirement on local and both NFS versions.

For prefix, the fixture's second zero-byte WRITE advances the real server's
session sequence without writing unacknowledged payload. NOSPC is a deliberate
synthetic protocol reply, not a claim that the server ran out of disk.
For uncached, real server effects intentionally exceed reported knowledge:
the readback sees bytes, while the failed input reports zero confirmed bytes.
Wire logs assert exact request counts and suffix offsets.
The successful SEQUENCE followed by an uncached error on the next operation
matches the cached-reply representation specified in
[RFC 8881 section 2.10.6.1.3](https://www.rfc-editor.org/rfc/rfc8881.html#section-2.10.6.1.3).
It intentionally allows later independent requests to use the advanced slot.

Cleanup belongs to the original handle/backend, including after Unknown.
Verification uses a fresh handle instead of treating the uncertain original
handle as safely reusable. Rust drops its owned handles; C frees its filesystem;
Python shuts down its client. Fresh verification handles are explicitly closed,
tested unusable where the API permits, and the fixture file is removed.

## Kernel publication and ownership rows

The same error model applies to io_uring, but a TCP proxy cannot simulate lost
kernel completion ownership. Existing deterministic fail_after_submit hooks
remain private to the engine tests; no public injection interface is added.
ci/test-uring-outcomes.sh requires these exact tests to execute on real rings:

| Test | Dispatch point | Expected state/progress | Retry | Cleanup owner |
|---|---|---|---|---|
| failure_after_publication_retains_inflight_ownership_without_replay | Submission published, CQEs unavailable | Unknown; one poisoned executor; bounded retained bytes/descriptors | No replay or new allocation arena | Retained kernel-facing owners; never free storage still potentially in use |
| fatal_write_submission_keeps_confirmed_prefix_and_untouched_suffix_distinct | Second two-byte wave loses publication/completion knowledge | Unknown with 2 confirmed bytes; untouched sibling NotAttempted without progress | Poisoned engine never submits again | Engine retains uncertain wave |
| sync_publication_loss_keeps_prior_success_and_unsubmitted_suffix | Second sync wave loses its reply | Prior success; Unknown; later NotAttempted | Subsequent calls remain NotAttempted | Engine retains uncertain descriptors |

C's extended_vectors_keep_siblings_and_confirmed_failed_prefixes and Python's
operation_exceptions_preserve_each_failure_state_and_owned_read_prefix test the
adapter projections of these shared OpError states and progress. C/Python
currently expose no io_uring constructor, so their live protocol rows use NFS;
the engine-specific lifetime assertions stay at the kernel boundary.

## Required gates and reproduction

The public CI package-archive gate compiles the consumer against the packaged
header and loader. It requires fallback using the checksum-pinned published
vfsi-c 0.3.11 library, plus missing-library POSIX fallback. Live CI requires C,
Rust and Python rows for both minors. The real-ring job requires all three
kernel rows, rejecting zero selected tests or ignored coverage.

~~~sh
ci/test-c-outcomes.sh
ci/test-c-outcomes.sh --server 127.0.0.1:2049 --minor 1
ci/test-c-outcomes.sh --server 127.0.0.1:2049 --minor 2
cargo build -p vfsi-nfs --example io-outcome-consumer
python3 ci/test-io-outcomes.py --adapter rust \
  --consumer target/debug/examples/io-outcome-consumer --minor 1
# Use a Python environment with the current native nfs4fs extension installed.
python3 ci/test-io-outcomes.py --adapter python \
  --consumer ci/python-outcome-consumer.py --minor 1
ci/test-uring-outcomes.sh
python3 -m unittest discover -s ci -p test_nfs_outcome_fixture.py
~~~

Repeat Rust/Python commands with --minor 2. --output NEW.json records actual
outcomes, wire requests, and consumer/library/fixture hashes without overwriting
prior evidence. The C wrapper accepts the same output argument for live rows.
Set --legacy-library to an already verified older SDK to reuse that binary;
otherwise the wrapper downloads, verifies and builds published 0.3.11.
