#!/usr/bin/env python3
"""Compare nfs4fs bulk I/O with LocalFileSystem over a kernel NFS mount.

The NFS export must be reachable both through nfs4fs and through a mounted
directory. Samples use fresh paths by default. Pass ``--reuse-paths`` to warm
both clients once and repeatedly access the same files.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import statistics
import time
import uuid
from pathlib import Path
from typing import Callable, TypeVar

import fsspec

T = TypeVar("T")


def _timed(call: Callable[[], T]) -> tuple[float, T]:
    started = time.perf_counter_ns()
    result = call()
    return (time.perf_counter_ns() - started) / 1_000_000, result


def _median(values: list[float | int]) -> float:
    return float(statistics.median(values))


def _remote_path(root: str, relative: str) -> str:
    root = root.strip("/")
    return f"/{root}/{relative}" if root else f"/{relative}"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--remote-root", default="")
    parser.add_argument("--direct-root", type=Path, default=Path("/export"))
    parser.add_argument("--mount-root", type=Path, default=Path("/mnt/nfs"))
    parser.add_argument("--files", type=int, default=20)
    parser.add_argument("--bytes", type=int, default=4096)
    parser.add_argument("--rounds", type=int, default=30)
    parser.add_argument("--reuse-paths", action="store_true")
    args = parser.parse_args()
    if args.files <= 0 or args.bytes <= 0 or args.rounds <= 0:
        parser.error("--files, --bytes, and --rounds must be positive")

    run_name = f"nfs4fs-benchmark-{uuid.uuid4().hex}"
    direct_run = args.direct_root / args.remote_root.strip("/") / run_name
    mount_run = args.mount_root / args.remote_root.strip("/") / run_name
    direct_run.mkdir(parents=True)

    nfs = fsspec.filesystem(
        "nfs4",
        host=args.host,
        root=args.remote_root,
        skip_instance_cache=True,
    )
    local = fsspec.filesystem("file", skip_instance_cache=True)
    payload = bytes((index % 251 for index in range(args.bytes)))

    nfs_write_ms: list[float] = []
    local_write_ms: list[float] = []
    nfs_read_ms: list[float] = []
    local_read_ms: list[float] = []
    write_compounds: list[int] = []
    read_compounds: list[int] = []
    write_rpcs: list[int] = []
    read_rpcs: list[int] = []

    try:
        probe_dir = direct_run / "two-file-probe"
        probe_dir.mkdir()
        probe_paths = [
            _remote_path(args.remote_root, f"{run_name}/two-file-probe/file-{i}")
            for i in (1, 2)
        ]
        nfs._client.compound_stats()
        nfs.pipe({probe_paths[0]: b"hello", probe_paths[1]: b"world"})
        two_file_pipe_compounds = nfs._client.compound_stats()[0]

        for round_number in range(args.rounds):
            path_suffix = "warm" if args.reuse_paths else str(round_number)
            nfs_write_rel = f"write-nfs4fs-{path_suffix}"
            local_write_rel = f"write-kernel-{path_suffix}"
            nfs_read_rel = f"read-nfs4fs-{path_suffix}"
            local_read_rel = f"read-kernel-{path_suffix}"
            for relative in (
                nfs_write_rel,
                local_write_rel,
                nfs_read_rel,
                local_read_rel,
            ):
                (direct_run / relative).mkdir(exist_ok=args.reuse_paths)

            nfs_write_paths = [
                _remote_path(
                    args.remote_root,
                    f"{run_name}/{nfs_write_rel}/file-{index:04}",
                )
                for index in range(args.files)
            ]
            local_write_paths = [
                str(mount_run / local_write_rel / f"file-{index:04}")
                for index in range(args.files)
            ]
            if args.reuse_paths and round_number == 0:
                nfs.pipe(dict.fromkeys(nfs_write_paths, payload))
                local.pipe(dict.fromkeys(local_write_paths, payload))

            def measure_nfs_write() -> None:
                nfs._client.compound_stats()
                nfs._client.rpc_stats()
                elapsed, _ = _timed(
                    lambda: nfs.pipe(dict.fromkeys(nfs_write_paths, payload))
                )
                nfs_write_ms.append(elapsed)
                write_compounds.append(nfs._client.compound_stats()[0])
                write_rpcs.append(nfs._client.rpc_stats()[0])

            def measure_local_write() -> None:
                elapsed, _ = _timed(
                    lambda: local.pipe(dict.fromkeys(local_write_paths, payload))
                )
                local_write_ms.append(elapsed)

            if round_number % 2:
                measure_local_write()
                measure_nfs_write()
            else:
                measure_nfs_write()
                measure_local_write()

            if not args.reuse_paths or round_number == 0:
                for index in range(args.files):
                    (direct_run / nfs_read_rel / f"file-{index:04}").write_bytes(
                        payload
                    )
                    (direct_run / local_read_rel / f"file-{index:04}").write_bytes(
                        payload
                    )
                os.sync()
            nfs_read_paths = [
                _remote_path(
                    args.remote_root,
                    f"{run_name}/{nfs_read_rel}/file-{index:04}",
                )
                for index in range(args.files)
            ]
            local_read_paths = [
                str(mount_run / local_read_rel / f"file-{index:04}")
                for index in range(args.files)
            ]
            if args.reuse_paths and round_number == 0:
                nfs.cat(nfs_read_paths)
                local.cat(local_read_paths)

            def measure_nfs_read() -> None:
                nfs._client.compound_stats()
                nfs._client.rpc_stats()
                elapsed, nfs_data = _timed(lambda: nfs.cat(nfs_read_paths))
                nfs_read_ms.append(elapsed)
                read_compounds.append(nfs._client.compound_stats()[0])
                read_rpcs.append(nfs._client.rpc_stats()[0])
                if len(nfs_data) != args.files or any(
                    value != payload for value in nfs_data.values()
                ):
                    raise RuntimeError("nfs4fs returned unexpected data")

            def measure_local_read() -> None:
                elapsed, local_data = _timed(lambda: local.cat(local_read_paths))
                local_read_ms.append(elapsed)
                if len(local_data) != args.files or any(
                    value != payload for value in local_data.values()
                ):
                    raise RuntimeError("LocalFileSystem returned unexpected data")

            if round_number % 2:
                measure_local_read()
                measure_nfs_read()
            else:
                measure_nfs_read()
                measure_local_read()
    finally:
        nfs.close()
        close_local = getattr(local, "close", None)
        if close_local is not None:
            close_local()
        shutil.rmtree(direct_run, ignore_errors=True)

    result = {
        "files": args.files,
        "bytes_per_file": args.bytes,
        "rounds": args.rounds,
        "path_mode": "warm" if args.reuse_paths else "cold",
        "two_file_pipe_compounds": two_file_pipe_compounds,
        "write": {
            "nfs4fs_median_ms": _median(nfs_write_ms),
            "kernel_nfs_median_ms": _median(local_write_ms),
            "speedup": _median(local_write_ms) / _median(nfs_write_ms),
            "nfs4fs_median_compounds": _median(write_compounds),
            "nfs4fs_median_rpcs": _median(write_rpcs),
        },
        "read": {
            "nfs4fs_median_ms": _median(nfs_read_ms),
            "kernel_nfs_median_ms": _median(local_read_ms),
            "speedup": _median(local_read_ms) / _median(nfs_read_ms),
            "nfs4fs_median_compounds": _median(read_compounds),
            "nfs4fs_median_rpcs": _median(read_rpcs),
        },
    }
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
