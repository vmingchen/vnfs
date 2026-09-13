# Vector batch result and error design

## Problems in the current contract

The legacy shape is generally:

```rust
fn operation_v(...) -> VfResult<Vec<T>>;
// or
fn operation_v(...) -> VfResult<()>;
```

It loses:

- Successful values preceding a later failure.
- Which requests were dispatched versus never sent.
- Chunk boundaries when a large vector was split.
- Partial progress within one logical operation.
- Resources created before failure, especially open handles.
- Whether a transport failure happened before or after dispatch.
- Protocol-operation location versus caller request index.

The default outcome adapters in
[`vfsi-sync/src/traits.rs`](../crates/vfsi-sync/src/traits.rs) therefore have to
infer states from only an error index. No NFS, SMB, or local backend currently
overrides those `*_outcomes` methods.

Meanwhile, NFS already has richer internal structures such as
`PathReadOutcome`, `PathWriteOutcome`, `PathOpenOutcome`,
`PathRemoveOutcome`, and `PathRenameOutcome` in
[`vfsi-nfs/src/client.rs`](../crates/vfsi-nfs/src/client.rs). That information
gets compressed when crossing the `VecFs` boundary.

Other notable problems:

- `OpOutcome::Completed` means success without the returned value. That is unusable for `OPEN` and weak for reads or writes.
- All transport errors currently imply mutation-style uncertainty, even though a read can normally be retried safely.
- A split batch can have confirmed previous chunks, an uncertain in-flight chunk, and definitely unattempted future chunks. The current adapters cannot represent that accurately.
- `VfError.index` mixes caller index, protocol operation index, and sometimes chunk-relative index.
- `closev` can remove handles from the local descriptor table before the server confirms `CLOSE`. A transport failure then leaves no usable ownership record.
- Callback and recursive APIs need progress/checkpoint reporting, not merely one result per top-level input.

## Recommended public model

Make exact outcomes the primitive backend contract. Derive fail-fast convenience APIs from them—not the reverse.

```rust
pub type BatchResult<T> =
    Result<BatchReport<T>, BatchRejected>;

pub struct BatchReport<T> {
    outcomes: Vec<ItemOutcome<T>>,
    execution: ExecutionSummary,
    termination: BatchTermination,
}

pub enum ItemOutcome<T> {
    Success(T),
    Failed(ItemFailure),
    Unknown(ItemUnknown),
    NotAttempted(NotAttemptedReason),
}
```

Remove `Completed`. A production public backend must never discard a successful returned value.

### Meanings

- `Success(T)`: authoritative success response received and its value retained.
- `Failed`: authoritative failure received. The failure records whether the logical item had no effect or partial effect.
- `Unknown`: the request was dispatched, but its final result or effect cannot be proven.
- `NotAttempted`: the backend can prove the item was never dispatched.

```rust
pub struct ItemFailure {
    pub error: FsError,
    pub effect: EffectEvidence,
    pub progress: Option<OperationProgress>,
}

pub enum EffectEvidence {
    NotApplied,
    PartiallyApplied,
}

pub struct ItemUnknown {
    pub error: FsError,
    pub retry: RetryAdvice,
}

pub enum RetryAdvice {
    Safe,
    ReconcileFirst,
    Never,
}
```

`Success` already means completely applied. A failed logical operation might still be partial: for example, `OPEN` could succeed but a following `GETFH` could fail, or a client-side copy could write some data before failing.

## Batch-level rejection versus execution failure

Use the outer `Result` only when nothing was dispatched:

```rust
pub enum BatchRejected {
    InvalidInput { /* mismatched lengths, bad flags */ },
    Unsupported,
    Disconnected,
    ClientState,
}
```

Once any request may have been dispatched, return a `BatchReport`, even if decoding subsequently fails. A malformed reply becomes `BatchFault`, while affected items become `Unknown`; it must not masquerade as a pre-dispatch error.

This cleanly separates:

```text
Batch rejected locally
    versus
Batch executed with per-item outcomes
```

## Better error location

Do not overload one index:

```rust
pub struct ErrorLocation {
    pub request_index: usize,
    pub chunk_index: Option<usize>,
    pub protocol_op_index: Option<usize>,
    pub phase: OperationPhase,
}
```

Possible phases include:

- `Validate`
- `ResolveParent`
- `Open`
- `GetFileHandle`
- `Read`
- `Write`
- `SetAttributes`
- `Close`
- `DecodeReply`
- `Cleanup`

This lets an application identify its request while preserving useful NFS COMPOUND or SMB command diagnostics.

## Operation categories

| Category | APIs | Required special handling |
|---|---|---|
| Read-only values | `readv`, `read_allv`, `getattrsv`, `lgetattrsv`, `readlinkv` | Preserve prefix values; transport retry normally safe |
| Resource lifecycle | `openv`, `closev` | Never lose ownership of live or uncertain handles |
| Data mutations | `writev`, `write_adb`, `dupv`, `ldupv`, `copyv` | Report byte/block progress and ambiguity |
| Metadata mutations | `setattrsv`, `lsetattrsv` | No replay after uncertain dispatch |
| Namespace mutations | `renamev`, `removev`, `unlinkv`, `mkdirv`, `symlinkv`, `hardlinkv`, `rm` | Preserve known prefix, failure, unsent suffix |
| Streaming | `read_streamv`, `listdirv` | Return per-file resume checkpoints and distinguish cancellation |
| Recursive jobs | `walk`, `cp_recursive`, recursive `rm` | Return a progress journal rather than one flat outcome |

`listdirv`, `read_streamv`, and recursive operations should not be forced into the same one-result-per-input abstraction. They need a specialized report:

```rust
pub struct StreamReport {
    pub completion: StreamCompletion,
    pub checkpoints: Vec<StreamCheckpoint>,
    pub error: Option<FsError>,
}
```

## Required backend rules

Every backend implementation should obey these invariants:

1. Validate the entire batch before dispatch where possible.
2. Return exactly one outcome per input item.
3. Never infer success merely from an error index unless the protocol guarantees it.
4. Mark `NotAttempted` only with positive evidence that the item was unsent.
5. Mark `Unknown` only for dispatched operations lacking an authoritative outcome.
6. Never discard a successful value required for cleanup or continued use.
7. Never silently replay an uncertain mutation.
8. Preserve confirmed results from earlier chunks.
9. Keep uncertain resources registered locally until reconciled or session teardown.
10. Include partial progress when one logical item spans multiple wire operations.

## NFS-specific implementation

For NFSv4:

- Use the existing `Path*Outcome` and execution-map machinery.
- A semantic COMPOUND failure produces:
  - successful prefix,
  - one failed logical item,
  - unattempted suffix.
- A `SEQUENCE` rejection before business operations means the items were not attempted.
- If a session slot can replay the same request through NFSv4.1 exactly-once semantics, retrieve the authoritative cached response.
- If replay/recovery cannot establish the result:
  - confirmed earlier chunks remain `Success`,
  - the in-flight mutation chunk becomes `Unknown`,
  - future chunks become `NotAttempted`.
- Register successful `OPEN` prefix handles before returning.
- For partial `OPEN` sequences, either recover and close the resource internally or retain it in a tracked quarantine; never leak it invisibly.
- Do not delete local `CLOSE` state until close is confirmed. Use states such as `Open`, `Closing`, and `CloseUnknown`.

SMB should follow the same public semantics while using SMB compound response statuses and durable-handle/reconnect evidence. The local backend can implement exact outcomes with a straightforward scalar loop.

## Public API shape

The native vector trait should require exact methods:

```rust
pub trait VectorBackend: FileSystem {
    fn open_batch(
        &mut self,
        requests: &[OpenRequest],
        policy: BatchPolicy,
    ) -> BatchResult<BackendHandle>;

    fn read_batch(
        &mut self,
        requests: &[ReadRequest],
        policy: BatchPolicy,
    ) -> BatchResult<ReadResult>;

    // ...
}
```

Support explicit execution policy:

```rust
pub enum BatchPolicy {
    StopOnError,
    ContinueIndependent,
}
```

Then ergonomic methods are derived:

```rust
let report = client.open_many_report(&requests)?;
let files = report.require_all()?;
```

`require_all()` should return an error that owns the original report. For `OPEN`, that means successful `FsFile` values remain RAII-owned and are automatically closed if the error/report is dropped.

## Migration path

1. Freeze `VecFs` as the C/legacy compatibility interface.
2. Introduce an exact-outcome native backend trait.
3. Make `BatchReport` enforce `outcomes.len() == requests.len()`.
4. Implement NFS using its existing `Path*Outcome` structures.
5. Implement local and SMB.
6. Derive fail-fast native conveniences from exact reports.
7. Deprecate the current default `*_outcomes` adapters and `Completed`.
8. Later introduce a versioned C representation if exact outcomes are needed through the C ABI.

The central design principle is: **backend methods must report execution evidence, not just an error index**. That provides correct recovery while preserving vectorization and makes `openv`, writes, namespace mutations, streaming, and recursive operations follow one coherent failure model.

## Batch-wide execution and termination information

`execution` and `termination` describe batch-wide information that does not
fit into any single item outcome.

### `execution`

`execution` records how the backend divided and dispatched the batch:

```rust
pub struct ExecutionSummary {
    pub policy: BatchPolicy,
    pub chunks: Vec<ChunkExecution>,
}

pub struct ChunkExecution {
    pub range: Range<usize>,
    pub state: ChunkState,
}
```

For example, 100 requests may be divided into three NFS COMPOUNDs:

```text
items  0..40   confirmed
items 40..80   transport response lost
items 80..100  never dispatched
```

This supports:

- Proving which items were dispatched.
- Correctly distinguishing `Unknown` from `NotAttempted`.
- Resuming at an appropriate chunk boundary.
- Observability: number of COMPOUNDs, retries, and splits.
- Testing compound packing and failure handling.
- Diagnosing why a nominally vectorized operation used many round trips.

Applications should not need `execution` for ordinary error handling; exact item outcomes remain authoritative. It is primarily diagnostics and recovery evidence.

### Backend faults

A `BatchTermination::BackendFault` represents a problem with the batch
executor itself rather than a normal filesystem failure for one item.

Examples include:

- Malformed or truncated server response.
- Backend returned the wrong number of results.
- NFS response could not be mapped back to request ranges.
- Internal planner invariant was violated.
- A partially created resource could not be cleaned up or quarantined.
- Response decoding failed after the request was dispatched.

This is different from:

```text
item 7: NFS4ERR_NOENT
```

That is an ordinary `ItemOutcome::Failed`.

A fault might instead produce:

```text
items 0..40: Success
items 40..80: Unknown
items 80..100: NotAttempted
termination: backend fault while decoding chunk 1
```

The report must still be returned because it contains important confirmed and uncertain outcomes. Returning only `Err(malformed_response)` would discard that information.

The explicit termination reason is:

```rust
pub struct BatchReport<T> {
    outcomes: Vec<ItemOutcome<T>>,
    execution: ExecutionSummary,
    termination: BatchTermination,
}

pub enum BatchTermination {
    Complete,
    StoppedOnItem { index: usize },
    TransportLost { chunk: usize },
    Cancelled,
    BackendFault(BatchFault),
}
```

This prevents ambiguity about how `fault` relates to the outcomes.

For normal users:

```rust
report.require_all()?;
```

For recovery or observability:

```rust
match report.termination() {
    BatchTermination::Complete => {}
    BatchTermination::TransportLost { chunk } => reconcile(&report),
    BatchTermination::BackendFault(fault) => report_bug(fault),
    _ => {}
}
```

So:

- `outcomes` answers: **what happened to each request?**
- `execution` answers: **what did the backend dispatch, and how?**
- `termination`/`fault` answers: **why did the batch stop?**

`execution` could be compact or feature-gated if allocation is a concern. At minimum, the backend should retain confirmed-prefix, in-flight, and undispatched boundaries; storing every chunk’s detailed telemetry is optional.
