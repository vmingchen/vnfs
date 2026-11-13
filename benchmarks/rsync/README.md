# Rsync sender-scan measurements

The published baseline uses run.py and baseline.json. Its exact source,
registry dependency, lockfile, output parity, fallback and capture gates
remain fixed. See ../../docs/rsync-benchmark.md.

For development, use development.py with explicit full SDK and port commit
hashes, a separate release target directory, and an optional explicit SDK
patch. It archives private source snapshots and reuses the same measurement
worker and isolated network lifecycle. Cohort tracing requires positive
aggregate cursor reuse, allowing the initial root scan to have no cursor.
The published parser still rejects that different trace format.

See ../../docs/rsync-development.md for measured inputs, scope, results and
the complete reproduction command. Run tests with:

    python3 -m unittest discover -s benchmarks/rsync

Never replace the published inputs or weaken their gates to run development.

For published SDK 0.3.18 real transfers, use adoption.py with the exact frozen
port and its stock upstream base. It covers updates, deletion, excludes,
symlinks, partial failures, interruption/resume, legacy/POSIX fallback and the
273-RPC scan regression. audit_adoption.py independently checks the recorded
recipes, manifests and wire captures. See ../../docs/rsync-adoption.md.
