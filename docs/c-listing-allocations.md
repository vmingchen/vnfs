# Compact C listing snapshots

The path-based C listing snapshot now retains projected C attributes, entry
names and borrowed operand-index groups. Scoped identities use their existing
optional parallel storage. It no longer clones full backend attributes and
a directory path for every row or builds a second prepared snapshot.

Callbacks still run after complete bounded collection and outside backend
locks. Duplicate operands retain their budget charges and output indices.
Backend errors, then quota errors, then projection errors retain their
previous precedence; the added regression covers all three together.

The opt-in allocation test creates 32 directories with 32 files each and long
names before counting allocation requests during the one listing call.

| Variant | Entries | Allocation requests | Requested bytes |
|---|---:|---:|---:|
| Before | 1,024 | 11,730 | 1,481,188 |
| Compact | 1,024 | 9,681 | 984,772 |

That is 17.5% fewer requests
and 33.5% fewer requested bytes.
These are successful Rust allocation/reallocation requests on the calling
thread, including requested reallocation sizes. They are not peak resident
memory, live heap size, libc allocations, or wire bytes.

Run the private ignored test explicitly:

```sh
cargo test -p vfsi-c --lib allocation_tests::indexed_listing_allocation_profile -- --ignored --exact --nocapture
```

[Raw measurements](../benchmarks/c-listing/allocations.json).
C ABI v3 and the checked-in public headers are unchanged.
