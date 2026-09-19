# Vector result and error design

## Scope

VFSI vector operations coalesce multiple filesystem requests. They are not
transactions: an error does not roll back operations that the server already
performed. Atomic commit and rollback belong to the future TFSI interface.

The application-facing Rust API uses the conventional strict shape:

```rust,ignore
fn openv(requests: &[OpenRequest]) -> Result<Vec<File>, VfError>;
```

`Ok` contains one value per input in input order. `Err` contains the first
observed failure and no partial value vector. For mutations, an error makes no
claim that earlier or concurrently dispatched operations were undone.

## Public and backend boundaries

Public vector methods use the `*v` vocabulary:

- `openv`
- `readv`
- `writev`
- `closev`
- `removev`
- `renamev`

Backends use `*_many` internally while planning compounds, concurrent SMB
requests, and local loops. The open implementation seam is:

```rust,ignore
fn open_many(
    paths: &[&Path],
    flags: &[i32],
    modes: &[u32],
) -> Result<ManyResults<VfFile>, VfError>;
```

`ManyResults<T>` is deliberately hidden from the application facade. Entry
`n` corresponds to request `n`. An ordered protocol may return a shorter
contiguous sequence after a semantic failure; an independent/concurrent
backend may retain results after an earlier request failed.

The public adapter chooses the lowest request-position failure, attaches that
position to `VfError`, disposes of every successful value when required, and
returns no partial vector.

## Error index

The input position is the authoritative vector index. Backend-local protocol
operation numbers and chunk offsets must be translated before an error crosses
the public boundary.

`VfError::index_opt()` returns:

- `Some(n)` when the failure is attributable to request `n`;
- `None` when a transport or client failure cannot be attributed reliably.

Code must not interpret an unknown index as request zero. The compatibility
`index()` accessor is retained temporarily, but new code uses `index_opt()`.

## OPEN resource ownership

`openv` has an additional ownership rule: if any request fails, every
confirmed successful handle is closed before the error returns. The original
open error remains primary if cleanup itself fails.

This is resource cleanup, not transaction rollback. `O_CREAT` may already
have created files, and an ambiguous lost reply may leave effects that the
client cannot prove. Potentially mutating opens such as `O_CREAT | O_EXCL`
must not be replayed blindly after an ambiguous transport failure.

## Backend behavior

### NFSv4

NFS COMPOUND executes in order and stops at the first failing operation. The
backend retains successful prefix handles and the semantic failure, while the
absent suffix means no result was reported. Compound splitting for negotiated
operation and byte limits preserves global request positions.

### SMB

Independent SMB requests may complete concurrently and out of order. The
backend stores their results in original request order. If public `openv`
observes any failure, it closes all handles from successful requests,
including requests that completed after the failing request.

### Local

The local backend executes in order and stops after the first failure. It is
the reference implementation for ordered `ManyResults` semantics.

## Fault-injection requirements

The `test-faults` feature exposes hidden deterministic hooks at these phases:

- before dispatch;
- after a reply;
- before local handle registration;
- after local handle registration;
- before cleanup.

Fault scripts are ordered and one-shot. Tests must assert that every configured
fault was consumed. Timing sleeps are not valid synchronization.

Required assertions include:

- every semantic failure position is reported exactly;
- unattributable transport failures retain `index_opt() == None`;
- no partial handle vector escapes;
- all confirmed successful handles are cleaned exactly once;
- cleanup failure does not mask the primary error;
- NFS success retains the compound-count fast path;
- SMB completion order does not change request ordering;
- no potentially mutating operation is automatically replayed after an
  ambiguous transport failure.

Property tests generate NFS-style prefixes and SMB-style complete result sets
for request counts and failure positions. Live NFS and Samba tests supplement
those deterministic tests with real protocol behavior. The NFS suite also
routes a live session through a test-only ONC-RPC record proxy, drops an OPEN
reply after server execution, and verifies that the mutating exclusive-create
batch is not replayed and remains unattributed to a fabricated request index.

## Migration

`open_many` is backend-only; the application facade exposes `openv` without a
public alias. Per-item outcome APIs are removed from the Rust-native API. The
same public-`*v`/internal-`*_many` split applies to the other native vector
operations.
