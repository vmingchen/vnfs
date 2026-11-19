# Actual-client standard-error comparison

Use the [measurement report](../../docs/benchmarks/error-integration/README.md)
for workload definitions, exact frozen sources, build/fixture instructions,
raw paired samples, limitations and reproduction commands. Current sources
require --features standard-error; no-feature compilation targets only the old
indexed source archive. Timing, allocation and wire controls are separate.
`run.py --controls-only` needs only the allocation binaries and does not
collect timing pairs.
