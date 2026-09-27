#!/usr/bin/env python3
"""Compare metadata-heavy tree walks and recursive removal on one NFS export.

Each trial creates two equivalent fresh trees outside the timed region. The
clients alternate order to reduce systematic first/second-run bias.
"""

from __future__ import annotations

import argparse
import json
import shutil
import statistics
import time
import uuid
from pathlib import Path

import fsspec


def _populate(root: Path, directories: int, files_per_dir: int, payload: bytes) -> None:
    for directory_index in range(directories):
        directory = root / f"dir-{directory_index:05d}"
        directory.mkdir(parents=True)
        for file_index in range(files_per_dir):
            (directory / f"file-{file_index:05d}").write_bytes(payload)


def _check_tree(entries: dict, directories: int, files_per_dir: int, size: int) -> None:
    files = [entry for entry in entries.values() if entry["type"] == "file"]
    dirs = [entry for entry in entries.values() if entry["type"] == "directory"]
    if len(files) != directories * files_per_dir or len(dirs) < directories:
        observed = [
            (Path(name).name, entry.get("type"))
            for name, entry in list(entries.items())[:8]
        ]
        raise RuntimeError(
            f"incomplete tree: {len(dirs)} directories, {len(files)} files; "
            f"first entries: {observed}"
        )
    if sum(entry["size"] for entry in files) != directories * files_per_dir * size:
        raise RuntimeError("tree has the wrong total file size")


def _summary(samples: list[float]) -> dict[str, float]:
    return {
        "median_ms": statistics.median(samples),
        "min_ms": min(samples),
        "max_ms": max(samples),
    }


def _wait_for_kernel_fixture(
    local,
    mounted_path: Path,
    fixture_path: Path,
    directories: int,
    files_per_dir: int,
    size: int,
) -> None:
    """Exclude cross-client visibility delay from the timed tree walk."""
    deadline = time.monotonic() + 5.0
    while True:
        try:
            _check_tree(
                local.find(str(mounted_path), withdirs=True, detail=True),
                directories,
                files_per_dir,
                size,
            )
            return
        except RuntimeError as error:
            if time.monotonic() >= deadline:
                raise RuntimeError(
                    f"kernel fixture did not become visible: {error}; "
                    f"mounted_exists={mounted_path.exists()}, "
                    f"server_exists={fixture_path.exists()}, "
                    f"server_children={list(fixture_path.iterdir()) if fixture_path.exists() else []}"
                ) from error
            time.sleep(0.1)


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
    parser.add_argument("--directories", type=int, default=64)
    parser.add_argument("--files-per-dir", type=int, default=4)
    parser.add_argument("--bytes-per-file", type=int, default=128)
    parser.add_argument("--rounds", type=int, default=5)
    args = parser.parse_args()
    if any(
        value <= 0
        for value in (
            args.directories,
            args.files_per_dir,
            args.bytes_per_file,
            args.rounds,
        )
    ):
        parser.error(
            "directories, files-per-dir, bytes-per-file, and rounds must be positive"
        )

    run_name = f"nfs4fs-directories-{uuid.uuid4().hex}"
    prefix = args.remote_root.strip("/")
    fixture_run = (args.fixture_root or args.mount_root) / prefix / run_name
    mounted_run = args.mount_root / prefix / run_name
    nfs = None
    local = None
    payload = bytes((i % 251 for i in range(args.bytes_per_file)))
    results: dict[str, dict[str, list[float | int]]] = {
        operation: {
            "nfs4fs_ms": [],
            "kernel_nfs_ms": [],
            "nfs4fs_compounds": [],
            "nfs4fs_rpcs": [],
        }
        for operation in ("find", "remove_recursive")
    }

    try:
        fixture_run.mkdir(parents=True)
        nfs = fsspec.filesystem(
            "nfs4",
            host=args.host,
            root=args.remote_root,
            auth="auth_sys",
            use_listings_cache=False,
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

        for round_number in range(args.rounds):
            suffixes = (f"nfs4fs-{round_number}", f"kernel-{round_number}")
            _populate(
                fixture_run / suffixes[0],
                args.directories,
                args.files_per_dir,
                payload,
            )
            # Create the kernel-side fixture through the mount. Creating it
            # behind the mount can leave a stale negative dentry/attribute
            # cache entry and make LocalFileSystem.rm misclassify the tree.
            _populate(
                mounted_run / suffixes[1],
                args.directories,
                args.files_per_dir,
                payload,
            )
            _wait_for_kernel_fixture(
                local,
                mounted_run / suffixes[1],
                fixture_run / suffixes[1],
                args.directories,
                args.files_per_dir,
                args.bytes_per_file,
            )
            paths = (
                f"/{run_name}/{suffixes[0]}",
                str(mounted_run / suffixes[1]),
            )

            def measure(operation: str, direct: bool) -> None:
                fs = nfs if direct else local
                path = paths[0] if direct else paths[1]
                if direct:
                    nfs._client.compound_stats()
                    nfs._client.rpc_stats()
                started = time.perf_counter_ns()
                if operation == "find":
                    entries = fs.find(path, withdirs=True, detail=True)
                else:
                    try:
                        fs.rm(path, recursive=True)
                    except IsADirectoryError:
                        if direct:
                            raise
                        # LocalFileSystem.rm probes isdir() before calling
                        # shutil.rmtree(). On an NFS mount, that probe can
                        # briefly return false even though unlink confirms
                        # the path is a directory. Use the same recursive
                        # deletion implementation after that specific race.
                        shutil.rmtree(path)
                    entries = None
                elapsed = (time.perf_counter_ns() - started) / 1_000_000
                compounds = nfs._client.compound_stats()[0] if direct else 0
                rpcs = nfs._client.rpc_stats()[0] if direct else 0
                if entries is not None:
                    _check_tree(
                        entries,
                        args.directories,
                        args.files_per_dir,
                        args.bytes_per_file,
                    )
                elif fs.exists(path):
                    raise RuntimeError(f"recursive removal left {path!r} behind")
                key = "nfs4fs_ms" if direct else "kernel_nfs_ms"
                results[operation][key].append(elapsed)
                if direct:
                    results[operation]["nfs4fs_compounds"].append(compounds)
                    results[operation]["nfs4fs_rpcs"].append(rpcs)

            for operation in ("find", "remove_recursive"):
                if round_number % 2:
                    measure(operation, False)
                    measure(operation, True)
                else:
                    measure(operation, True)
                    measure(operation, False)
    finally:
        if nfs is not None:
            nfs.close()
        close_local = getattr(local, "close", None) if local is not None else None
        if close_local is not None:
            close_local()
        shutil.rmtree(fixture_run, ignore_errors=True)

    report = {
        "benchmark": "directory_tree",
        "directories": args.directories,
        "files_per_directory": args.files_per_dir,
        "bytes_per_file": args.bytes_per_file,
        "rounds": args.rounds,
        "fixture_visibility_wait_excluded": True,
        "operations": {},
    }
    for operation, samples in results.items():
        direct_ms = samples["nfs4fs_ms"]
        kernel_ms = samples["kernel_nfs_ms"]
        report["operations"][operation] = {
            "nfs4fs": {
                **_summary(direct_ms),
                "median_compounds": statistics.median(samples["nfs4fs_compounds"]),
                "median_rpcs": statistics.median(samples["nfs4fs_rpcs"]),
            },
            "kernel_nfs": _summary(kernel_ms),
            "speedup": statistics.median(kernel_ms) / statistics.median(direct_ms),
        }
    print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
