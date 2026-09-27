#!/usr/bin/env python3
"""Compare bounded sequential reads through nfs4fs and a kernel NFS mount.

Both clients read the same file. Fixture creation and warm-ups are outside the
timed region. This is a warm/cold-*client* comparison, not a claim that server
or kernel caches have been globally cleared.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import statistics
import time
import uuid
from pathlib import Path

import fsspec


def _read_stream(fs, path: str, chunk_bytes: int, *, direct: bool) -> tuple[int, str]:
    digest = hashlib.blake2b()
    total = 0
    kwargs = {"cache_type": "none"} if direct else {}
    with fs.open(path, "rb", **kwargs) as stream:
        while chunk := stream.read(chunk_bytes):
            digest.update(chunk)
            total += len(chunk)
    return total, digest.hexdigest()


def _summary(samples: list[float], size_bytes: int) -> dict[str, float]:
    median_ms = statistics.median(samples)
    return {
        "median_ms": median_ms,
        "min_ms": min(samples),
        "max_ms": max(samples),
        "median_mib_per_s": size_bytes / (1024 * 1024) / (median_ms / 1000),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--remote-root", default="")
    parser.add_argument("--mount-root", type=Path, default=Path("/mnt/nfs"))
    parser.add_argument(
        "--fixture-root",
        type=Path,
        help="Server-local path to the same export (defaults to --mount-root)",
    )
    parser.add_argument("--size-mib", type=int, default=256)
    parser.add_argument("--chunk-kib", type=int, default=1024)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--warmups", type=int, default=1)
    args = parser.parse_args()
    if args.size_mib <= 0 or args.chunk_kib <= 0 or args.rounds <= 0:
        parser.error("--size-mib, --chunk-kib, and --rounds must be positive")
    if args.warmups < 0:
        parser.error("--warmups must be non-negative")

    run_name = f"nfs4fs-large-{uuid.uuid4().hex}"
    prefix = args.remote_root.strip("/")
    fixture_run = (args.fixture_root or args.mount_root) / prefix / run_name
    mounted_run = args.mount_root / prefix / run_name
    remote_file = f"/{run_name}/large.bin"
    mounted_file = str(mounted_run / "large.bin")
    size_bytes = args.size_mib * 1024 * 1024
    chunk_bytes = args.chunk_kib * 1024
    nfs = None
    local = None
    nfs_ms: list[float] = []
    kernel_ms: list[float] = []
    nfs_rpcs: list[int] = []
    nfs_compounds: list[int] = []

    try:
        fixture_run.mkdir(parents=True)
        nfs = fsspec.filesystem(
            "nfs4",
            host=args.host,
            root=args.remote_root,
            auth="auth_sys",
            skip_instance_cache=True,
        )
        local = fsspec.filesystem("file", skip_instance_cache=True)
        probe = b"same-export-" + run_name.encode()
        (fixture_run / "probe").write_bytes(probe)
        if nfs.cat_file(f"/{run_name}/probe") != probe:
            raise RuntimeError("nfs4fs does not see the fixture export")
        if local.cat_file(str(mounted_run / "probe")) != probe:
            raise RuntimeError("kernel mount does not see the fixture export")
        (fixture_run / "probe").unlink()

        pattern = bytes(range(256)) * 4096  # exactly 1 MiB, kept bounded
        expected = hashlib.blake2b()
        with (fixture_run / "large.bin").open("wb") as output:
            for _ in range(args.size_mib):
                output.write(pattern)
                expected.update(pattern)
            output.flush()
            os.fsync(output.fileno())
        expected_result = (size_bytes, expected.hexdigest())

        for _ in range(args.warmups):
            if (
                _read_stream(nfs, remote_file, chunk_bytes, direct=True)
                != expected_result
            ):
                raise RuntimeError("nfs4fs warm-up returned incorrect data")
            if (
                _read_stream(local, mounted_file, chunk_bytes, direct=False)
                != expected_result
            ):
                raise RuntimeError("kernel NFS warm-up returned incorrect data")

        for index in range(args.rounds):

            def measure(fs, path: str, direct: bool) -> tuple[float, int, int]:
                if direct:
                    nfs._client.compound_stats()
                    nfs._client.rpc_stats()
                started = time.perf_counter_ns()
                result = _read_stream(fs, path, chunk_bytes, direct=direct)
                elapsed = (time.perf_counter_ns() - started) / 1_000_000
                if result != expected_result:
                    raise RuntimeError(
                        f"incorrect data from {'nfs4fs' if direct else 'kernel NFS'}"
                    )
                if direct:
                    return (
                        elapsed,
                        nfs._client.compound_stats()[0],
                        nfs._client.rpc_stats()[0],
                    )
                return elapsed, 0, 0

            if index % 2:
                kernel_ms.append(measure(local, mounted_file, False)[0])
                elapsed, compounds, rpcs = measure(nfs, remote_file, True)
            else:
                elapsed, compounds, rpcs = measure(nfs, remote_file, True)
                kernel_ms.append(measure(local, mounted_file, False)[0])
            nfs_ms.append(elapsed)
            nfs_compounds.append(compounds)
            nfs_rpcs.append(rpcs)
    finally:
        if nfs is not None:
            nfs.close()
        close_local = getattr(local, "close", None) if local is not None else None
        if close_local is not None:
            close_local()
        shutil.rmtree(fixture_run, ignore_errors=True)

    print(
        json.dumps(
            {
                "benchmark": "single_large_file_sequential_read",
                "size_bytes": size_bytes,
                "chunk_bytes": chunk_bytes,
                "rounds": args.rounds,
                "warmups": args.warmups,
                "nfs4fs": {
                    **_summary(nfs_ms, size_bytes),
                    "median_compounds": statistics.median(nfs_compounds),
                    "median_rpcs": statistics.median(nfs_rpcs),
                },
                "kernel_nfs": _summary(kernel_ms, size_bytes),
                "speedup": statistics.median(kernel_ms) / statistics.median(nfs_ms),
            },
            indent=2,
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
