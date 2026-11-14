# Standard-error migration: ownership and attribution inventory

Baseline: `70bea6a`. Stage 1 records the contracts before the public cutover.
The [design](vferror-simplification.md) is authoritative. The deferred NFS bitmap
decoder work remains outside this migration.

## Ownership decisions

| Current owners / examples | Migration decision |
| --- | --- |
| `vfsi-core/outcome.rs`, read/write progress transformations | Move the cause and owned progress through `into_parts`; retain states and exact prefix buffers. Remove cause-dependent Clone/Eq derives. |
| `vfsi-core/internal.rs::vec_result_slots`, sync client lock failures, Auto route rejection | Outer subgroup rejection becomes NotAttempted slots. Zero slots drop the cause, one slot moves it, multiple slots explicitly share an opaque original. |
| NFS/SMB per-response status construction | Reconstruct from known Copy protocol status and provenance; never reinterpret protocol codes as OS errno. |
| NFS recovery retained outcomes and reconnect notifications | Keep outcomes in place. Use explicit group sharing where the diagnostic is also needed by recovery/events; remove notification Clone requirements if not independently useful. No retry or resubmission changes. |
| NFS/SMB stopped cohorts, sync write dependencies and removal helpers | A stopped cause can affect multiple later slots with different states. Store an explicit native recipe or scoped shared source; never clone only ErrorKind or Display text. |
| `vfsi-sync/client/shared.rs` cleanup diagnostics and returned failure | Both owners need the actual cause. Share explicitly, retaining pending handles separately; do not make the diagnostics queue a handle owner. |
| `vfsi-core/api/{listdir,extensions}.rs`, `vnfs/helpers/transfer.rs` callback preservation | Move the original callback error into its saved slot; return a distinct private cancellation marker or explicitly shared source to the dispatcher when needed. Preserve callback failure selection and nonreplay. |
| `vfsi-core/api/io.rs` pending errors | Process state and confirmed prefix before moving the cause into the pending cursor. Preserve interruption suppression after a delivered prefix, unresolved write cursors, and no replay. |
| `vfsi-core/internal.rs::faults::FaultScript` | Already consumes each queued error exactly once; preserve this move-based behavior. |
| Test fake backends, cached failures, detached Python test support | Use one-shot owned errors where faults fire once. Repeated failures store explicit known recipes or a shared original. Tests must not require Clone/Eq on arbitrary public causes. |
| Python ErrorMap / C outcome slots | Map keys/positions own attribution. Move causes; use explicit optional index conversion. Retain confirmed-close ownership. |

Do not put Arc around every independent error. A shared-source payload in an
owned io::Error still has per-slot boxing; measure it. No error reconstruction
may repeat filesystem work or discard an opaque source.

## Attribution decisions

- Canonical VecResult: vector position, never a cause index.
- Pre-dispatch outer rejection: unattributed, including aggregate budget errors.
- Lossy collection: one IndexedError selected in input order; None remains None
  through remapping. Malformed cardinality has no attributed input.
- Route/chunk/native dependency maps: explicit original-slot maps. Wire offsets
  remain backend data and are translated only by the submitting code.
- Transfer/root/declaration helpers: IndexedError mapped once to original input.
- Scalar wrappers: explicitly discard selection attribution, retaining diagnostics.
- C/Python: explicit validation index, accepted slot, wrapper or map key; otherwise
  the existing unknown representation. Never mine sources for logical indices.

## Characterization evidence

The focused `error_contract`, `vector_results` and `allocations` targets cover:
exact native category/status, local timeout versus RPC provenance, non-Clone
opaque source downcasting/lifetime, context updates, independent siblings,
outer versus input-zero attribution, source retention across subgroup fan-out,
owned progress pointer identity, and allocation-free simple OS/category causes.
Existing cleanup, cardinality, progress validation and uncertainty tests remain.
These are baseline tests, not evidence that the migration is already complete.

Historical measurement inputs are frozen under
`docs/benchmarks/error-representations/source-2026-11-14`, with SHA-256 inventory
and the original raw results. Later benchmark adaptations must not replace that
source or claim those samples came from the integrated migration.
