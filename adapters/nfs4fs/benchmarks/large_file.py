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


def _read_pipelined(fs, path: str, chunk_bytes: int, workers: int, in_flight: int):
    """Hash ordered chunks delivered by nfs4fs's bounded read-ahead API."""
    digest = hashlib.blake2b()
    total = fs.read_stream_pipelined(
        path,
        lambda _offset, data: digest.update(data),
        workers=workers,
        chunk_size=chunk_bytes,
        max_in_flight=in_flight,
    )
    return total, digest.hexdigest()


def _pool_stats(fs):
    """Read reset-on-read counters for every native session in a pool."""
    clients = fs._client._clients
    return (
        sum(client.compound_stats()[0] for client in clients),
        sum(client.rpc_stats()[0] for client in clients),
    )


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
    parser.add_argument(
        "--pipeline-workers",
        type=int,
        default=0,
        help="also profile bounded positional read-ahead using this many sessions",
    )
    parser.add_argument("--in-flight", type=int, default=8)
    args = parser.parse_args()
    if args.size_mib <= 0 or args.chunk_kib <= 0 or args.rounds <= 0:
        parser.error("--size-mib, --chunk-kib, and --rounds must be positive")
    if args.warmups < 0:
        parser.error("--warmups must be non-negative")
    if args.pipeline_workers < 0 or args.in_flight <= 0:
        parser.error("--pipeline-workers must be non-negative and --in-flight positive")
    if args.pipeline_workers and args.chunk_kib > 1024:
        parser.error("pipelined reads currently require --chunk-kib <= 1024")
    if args.pipeline_workers and args.in_flight * args.chunk_kib > 16 * 1024:
        parser.error("pipelined read-ahead must stay within 16 MiB")

    run_name = f"nfs4fs-large-{uuid.uuid4().hex}"
    prefix = args.remote_root.strip("/")
    fixture_run = (args.fixture_root or args.mount_root) / prefix / run_name
    mounted_run = args.mount_root / prefix / run_name
    remote_file = f"/{run_name}/large.bin"
    mounted_file = str(mounted_run / "large.bin")
    size_bytes = args.size_mib * 1024 * 1024
    chunk_bytes = args.chunk_kib * 1024
    nfs = None
    nfs_pool = None
    local = None
    nfs_ms: list[float] = []
    kernel_ms: list[float] = []
    nfs_rpcs: list[int] = []
    nfs_compounds: list[int] = []
    pipeline_ms: list[float] = []
    pipeline_compounds: list[int] = []
    pipeline_rpcs: list[int] = []

    try:
        fixture_run.mkdir(parents=True)
        nfs = fsspec.filesystem(
            "nfs4",
            host=args.host,
            root=args.remote_root,
            auth="auth_sys",
            skip_instance_cache=True,
        )
        if args.pipeline_workers:
            nfs_pool = fsspec.filesystem(
                "nfs4",
                host=args.host,
                root=args.remote_root,
                auth="auth_sys",
                connection_pool_size=args.pipeline_workers,
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
            if nfs_pool is not None and (
                _read_pipelined(
                    nfs_pool,
                    remote_file,
                    chunk_bytes,
                    args.pipeline_workers,
                    args.in_flight,
                )
                != expected_result
            ):
                raise RuntimeError("pipelined nfs4fs warm-up returned incorrect data")

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

            def measure_sequential():
                elapsed, compounds, rpcs = measure(nfs, remote_file, True)
                nfs_ms.append(elapsed)
                nfs_compounds.append(compounds)
                nfs_rpcs.append(rpcs)

            def measure_pipeline():
                _pool_stats(nfs_pool)
                started = time.perf_counter_ns()
                result = _read_pipelined(
                    nfs_pool,
                    remote_file,
                    chunk_bytes,
                    args.pipeline_workers,
                    args.in_flight,
                )
                elapsed = (time.perf_counter_ns() - started) / 1_000_000
                if result != expected_result:
                    raise RuntimeError("pipelined nfs4fs returned incorrect data")
                compounds, rpcs = _pool_stats(nfs_pool)
                pipeline_ms.append(elapsed)
                pipeline_compounds.append(compounds)
                pipeline_rpcs.append(rpcs)

            actions = [
                measure_sequential,
                lambda: kernel_ms.append(measure(local, mounted_file, False)[0]),
            ]
            if nfs_pool is not None:
                actions.append(measure_pipeline)
            order = index % len(actions)
            for action in actions[order:] + actions[:order]:
                action()
    finally:
        if nfs is not None:
            nfs.close()
        if nfs_pool is not None:
            nfs_pool.close()
        close_local = getattr(local, "close", None) if local is not None else None
        if close_local is not None:
            close_local()
        shutil.rmtree(fixture_run, ignore_errors=True)

    report = {
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
    }
    if pipeline_ms:
        report["nfs4fs_pipelined"] = {
            **_summary(pipeline_ms, size_bytes),
            "workers": args.pipeline_workers,
            "max_in_flight": args.in_flight,
            "median_compounds": statistics.median(pipeline_compounds),
            "median_rpcs": statistics.median(pipeline_rpcs),
            "speedup_vs_sequential": statistics.median(nfs_ms)
            / statistics.median(pipeline_ms),
        }
    print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
