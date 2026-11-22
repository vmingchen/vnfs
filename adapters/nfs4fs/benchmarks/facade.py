"""Measure Python facade, buffered reads, and persistent cache paths."""

import argparse
import json
import platform
import statistics
import tempfile
import time
from pathlib import Path

import fsspec
import nfs4fs
from fsspec.core import OpenFile, OpenFiles

CASES = (
    "cat",
    "open-scalar",
    "open-grouped",
    "readinto",
    "cached-scalar",
    "cached-checked",
    "cached-grouped",
    "cached-partial",
    "cache-fill",
)


def grouped_read(fs, paths):
    with OpenFiles(
        [OpenFile(fs, path, mode="rb") for path in paths], mode="rb", fs=fs
    ) as files:
        return [file.read() for file in files]


def scalar_read(fs, paths, *, size=-1, block_size=None):
    contents = []
    for path in paths:
        with fs.open(path, "rb", block_size=block_size) as file:
            contents.append(file.read(size))
    return contents


def read_into(fs, paths, buffers):
    for path, buffer in zip(paths, buffers):
        with fs.open(path, "rb") as file:
            offset = 0
            while offset < len(buffer):
                remaining = memoryview(buffer)[offset:] if offset else buffer
                count = file.readinto(remaining)
                if not count:
                    raise RuntimeError(f"short read into buffer for {path}")
                offset += count
    return buffers


def persistent_cache(fs, storage, *, check_files=False):
    return fsspec.filesystem(
        "blockcache",
        fs=fs,
        cache_storage=str(storage),
        check_files=check_files,
        cache_check=0,
        skip_instance_cache=True,
    )


def rpc_count(fs):
    return fs._client.rpc_stats()[0]


def run(fs, scratch, args):
    paths = [f"/f{index:06d}" for index in range(args.files)]
    expected = [bytes([index % 251]) * args.bytes for index in range(args.files)]
    fs.pipe(dict(zip(paths, expected)))
    block_size = args.bytes // 2
    partial = [data[:block_size] for data in expected]
    buffers = [bytearray(args.bytes) for _ in paths]
    complete = persistent_cache(fs, scratch / "complete")
    checked = persistent_cache(fs, scratch / "checked", check_files=True)
    incomplete = persistent_cache(fs, scratch / "partial")
    fresh = persistent_cache(fs, scratch / "fresh")
    operations = {
        "cat": lambda: list(fs.cat(paths).values()),
        "open-scalar": lambda: scalar_read(fs, paths),
        "open-grouped": lambda: grouped_read(fs, paths),
        "readinto": lambda: read_into(fs, paths, buffers),
        "cached-scalar": lambda: scalar_read(complete, paths),
        "cached-checked": lambda: scalar_read(checked, paths),
        "cached-grouped": lambda: grouped_read(complete, paths),
        "cached-partial": lambda: scalar_read(
            incomplete, paths, size=block_size, block_size=block_size
        ),
        "cache-fill": lambda: grouped_read(fresh, paths),
    }
    if {"cached-scalar", "cached-grouped"} & set(args.cases):
        if grouped_read(complete, paths) != expected:
            raise RuntimeError("complete cache warm-up returned wrong contents")
    if "cached-checked" in args.cases:
        if grouped_read(checked, paths) != expected:
            raise RuntimeError("checked cache warm-up returned wrong contents")
    if "cached-partial" in args.cases:
        if operations["cached-partial"]() != partial or any(
            incomplete._metadata.cached_files[-1][path]["blocks"] != {0}
            for path in paths
        ):
            raise RuntimeError("partial cache warm-up did not leave one block per file")

    results = {case: [] for case in args.cases}
    for round_index in range(-args.warmups, args.rounds):
        order = args.cases if round_index % 2 == 0 else reversed(args.cases)
        for case in order:
            if case == "cache-fill":
                fresh.clear_cache()
            if args.host:
                rpc_count(fs)
            wall_start, cpu_start = time.perf_counter_ns(), time.process_time_ns()
            contents = operations[case]()
            cpu_ns, wall_ns = (
                time.process_time_ns() - cpu_start,
                time.perf_counter_ns() - wall_start,
            )
            rpcs = rpc_count(fs) if args.host else None
            if contents != (partial if case == "cached-partial" else expected):
                raise RuntimeError(f"{case} returned unexpected contents")
            if round_index >= 0:
                results[case].append(
                    {"wall_ns": wall_ns, "cpu_ns": cpu_ns, "rpcs": rpcs}
                )
    return {
        case: {
            "wall_ns_median": statistics.median(row["wall_ns"] for row in samples),
            "cpu_ns_median": statistics.median(row["cpu_ns"] for row in samples),
            "rpcs_median": (
                statistics.median(row["rpcs"] for row in samples) if args.host else None
            ),
            "samples": samples,
        }
        for case, samples in results.items()
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", help="optional NFS host[:port]; otherwise use dummy")
    parser.add_argument("--remote-root", default="", help="parent for an NFS fixture")
    parser.add_argument("--files", type=int, default=32)
    parser.add_argument("--bytes", type=int, default=512)
    parser.add_argument("--rounds", type=int, default=15)
    parser.add_argument("--warmups", type=int, default=3)
    parser.add_argument("--cases", nargs="+", choices=CASES, default=CASES)
    args = parser.parse_args()
    if args.files < 1 or args.bytes < 2 or args.rounds < 1 or args.warmups < 0:
        parser.error("files and rounds must be positive, bytes >= 2, and warmups >= 0")
    if len(set(args.cases)) != len(args.cases):
        parser.error("cases must be distinct")

    with tempfile.TemporaryDirectory(prefix="nfs4fs-facade-") as temporary:
        scratch = Path(temporary)
        if args.host:
            parent = args.remote_root.strip("/")
            root = "/".join(part for part in (parent, scratch.name) if part)
            with nfs4fs.Nfs4FileSystem(host=args.host, auth="auth_sys") as management:
                management.mkdir(root, create_parents=False)
                try:
                    with nfs4fs.Nfs4FileSystem(
                        host=args.host, root=root, auth="auth_sys", cache_type="none"
                    ) as fs:
                        results = run(fs, scratch, args)
                finally:
                    management.rm(root, recursive=True)
        else:
            with nfs4fs.Nfs4FileSystem(
                backend="dummy", dummy_root=str(scratch / "files"), cache_type="none"
            ) as fs:
                results = run(fs, scratch, args)

    print(
        json.dumps(
            {
                "settings": vars(args),
                "python": platform.python_version(),
                "nfs4fs": nfs4fs.__version__,
                "fsspec": fsspec.__version__,
                "results": results,
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
