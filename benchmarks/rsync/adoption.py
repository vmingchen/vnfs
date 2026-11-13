#!/usr/bin/env python3
"""Published-SDK rsync real-transfer conformance; frozen scan inputs stay intact."""

import argparse
import ctypes
from contextlib import contextmanager, ExitStack
import fnmatch
import hashlib
import pwd
import re
import shutil
import signal
import stat
import sys
import tempfile
import time
import gzip
import json
import os
from pathlib import Path
import subprocess

# Frozen helpers add their du directory to sys.path. Keep that private to the
# import so unittest discovery cannot accidentally load du/test_benchmark.py.
_import_path = sys.path[:]
try:
    import development
finally:
    sys.path[:] = _import_path

scan = development.scan
PORT = "7f4f8f87a58510acfcc71fc28c3461138327b848"
STOCK = "ea645201fefbba7f6b6b22b621856dba3c01cdd3"
SDK = "0.3.18"
SDK_SHA = "a8cdd7c5e57b3d46dc42fe7603b1180c44ea1e5ad95e185f71596531bb7b472c"


def prepare(args):
    args.output.mkdir(parents=True, exist_ok=False)
    port, port_evidence = development.snapshot(args.checkout, PORT, args.output, "port")
    stock, stock_evidence = development.snapshot(
        args.checkout, STOCK, args.output, "stock"
    )
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("VFSI_", "VNFS_"))
    }
    env["VFSI_BUILD_DIR"] = str(args.output / "release")
    with (args.output / "build.log").open("w") as log:
        subprocess.run(
            ["support/build-vfsi-c", "--headers-only"],
            cwd=port,
            env=env,
            stdout=log,
            stderr=log,
            check=True,
            timeout=300,
        )
        release = args.output / "release" / ("vfsi-c-" + SDK)
        archive, source = release / ("vfsi-c-" + SDK + ".crate"), release / "source"
        assert scan.sha(archive) == SDK_SHA
        lock_sha = scan.sha(source / "Cargo.lock")
        prefix = args.output / "sdk"
        subprocess.run(
            [
                "python3",
                str(source / "build-sdk.py"),
                "--registry-only",
                "--profile",
                "release",
                "--target-dir",
                str(args.target_dir),
                "--prefix",
                str(prefix),
            ],
            cwd=source,
            env=env,
            stdout=log,
            stderr=log,
            check=True,
            timeout=1800,
        )
        assert scan.sha(source / "Cargo.lock") == lock_sha
        versions = json.loads((prefix / "versions.json").read_text())
        assert versions["vfsi-c"] == SDK
        assert versions["vfsi-nfs"] == "0.2.3"
        for name in ("vfsi-core", "vfsi-sync", "vfsi-local", "vfsi-posix", "vfsi-smb"):
            assert versions[name] == "0.2.1", (name, versions)
        flags = [
            "--disable-md2man",
            "--disable-xxhash",
            "--disable-zstd",
            "--disable-lz4",
            "--disable-idn",
        ]
        for source_tree, extra, include in (
            (stock, [], ""),
            (port, ["--enable-vfsi"], "-I" + str(prefix / "include")),
        ):
            subprocess.run(
                ["./configure", *flags, *extra],
                cwd=source_tree,
                env=dict(env, CPPFLAGS=include),
                stdout=log,
                stderr=log,
                check=True,
                timeout=300,
            )
            subprocess.run(
                ["make", "-j2"],
                cwd=source_tree,
                stdout=log,
                stderr=log,
                env=env,
                check=True,
                timeout=1200,
            )
    library = prefix / "lib/libvfsi_c.so"
    result = dict(
        schema_version=1,
        port=port_evidence,
        stock=stock_evidence,
        sdk=SDK,
        sdk_archive_sha256=SDK_SHA,
        sdk_lock_sha256=lock_sha,
        versions=versions,
        configure_flags=flags,
        binary=str(port / "rsync"),
        stock_binary=str(stock / "rsync"),
        library=str(library),
        binary_sha256=scan.sha(port / "rsync"),
        stock_binary_sha256=scan.sha(stock / "rsync"),
        library_sha256=scan.sha(library),
        compiler=scan.checked(["cc", "--version"]).splitlines()[0],
        rustc=scan.checked(["rustc", "-Vv"]),
    )
    (args.output / "provenance.json").write_text(json.dumps(result, indent=2) + "\n")
    with (args.output / "build.log").open("rb") as source, gzip.open(
        args.output / "build.log.gz", "wb"
    ) as target:
        import shutil

        shutil.copyfileobj(source, target)
    (args.output / "build.log").unlink()
    print(json.dumps(result, indent=2), flush=True)


def checked_trace(text, method, *, partial=False):
    entries = retained = 0
    missing_diagnostic = False
    modes = set()
    for line in text.splitlines():
        if not line:
            continue
        if method == "missing-library" and re.fullmatch(
            r"vfsi: cannot load /nonexistent-vfsi-adoption\.so: .+", line
        ):
            missing_diagnostic = True
            continue
        match = re.fullmatch(
            r"vfsi: listdirs completed mode=(cohort|paths) dirs=(\d+) "
            r"cursors=(\d+) entries=(\d+) phase=collect cohort_application_entries=0",
            line,
        )
        if not match:
            raise ValueError("unexpected trace/diagnostic: " + line)
        mode, directories, cursors, count = match.groups()
        if not 1 <= int(directories) <= 32 or not 0 <= int(cursors) <= int(directories):
            raise ValueError("invalid cohort accounting")
        modes.add(mode)
        entries += int(count)
        retained += int(cursors)
    if method == "vfsi":
        if not entries or modes != {"cohort"} or (not partial and not retained):
            raise ValueError("missing positive retained-cohort evidence")
    elif method == "old-library":
        if not entries or modes != {"paths"} or retained:
            raise ValueError("old-library feature fallback did not use legacy paths")
    elif entries or modes:
        raise ValueError("stock/POSIX fallback unexpectedly accelerated")
    if method == "missing-library" and not missing_diagnostic:
        raise ValueError("missing-library fallback lacks its pre-dispatch load failure")
    return entries


def tree_manifest(root):
    """Content, type and permissions; never follow a source symlink."""
    root = Path(root)
    rows = {}
    for parent, directories, files in os.walk(root, followlinks=False):
        for path in [
            Path(parent),
            *(Path(parent) / name for name in sorted(directories + files)),
        ]:
            relative = str(path.relative_to(root))
            info = path.lstat()
            mode = stat.S_IMODE(info.st_mode)
            if stat.S_ISLNK(info.st_mode):
                value = ["symlink", mode, os.readlink(path)]
            elif stat.S_ISREG(info.st_mode):
                value = ["file", mode, info.st_size, scan.sha(path)]
            elif stat.S_ISDIR(info.st_mode):
                value = ["directory", mode]
            else:
                raise ValueError("unsupported fixture object: " + str(path))
            rows[relative] = value
    return dict(sorted(rows.items()))


def save_manifest(path, rows):
    data = (json.dumps(rows, sort_keys=True, separators=(",", ":")) + "\n").encode()
    with gzip.open(path, "wb") as stream:
        stream.write(data)
    return hashlib.sha256(data).hexdigest()


def expected_manifest(source, *, excluded=False, blocked=False):
    rows = tree_manifest(source)
    return {
        name: value
        for name, value in rows.items()
        if not (
            excluded
            and (
                "skip" in Path(name).parts or fnmatch.fnmatch(Path(name).name, "*.skip")
            )
        )
        and not (blocked and name == "blocked.bin")
    }


def put(path, data, *, mode=0o644):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    path.chmod(mode)
    os.utime(path, ns=(1700000002000000000, 1700000002000000000))


def partial_bytes(root):
    total = 0
    for parent, _, files in os.walk(root, followlinks=False):
        for name in files:
            try:
                info = (Path(parent) / name).lstat()
            except FileNotFoundError:
                continue
            if stat.S_ISREG(info.st_mode):
                total += info.st_size
    return total


def start_capture(interface, stem):
    capture_log = Path(str(stem) + ".capture.log")
    with capture_log.open("w") as log:
        process = subprocess.Popen(
            [
                "tcpdump",
                "--immediate-mode",
                "-n",
                "-B",
                "16384",
                "-i",
                interface,
                "-s",
                "0",
                "-w",
                str(stem) + ".pcap",
                "tcp",
                "port",
                "2049",
            ],
            stdout=subprocess.DEVNULL,
            stderr=log,
        )
    deadline = time.monotonic() + 10
    while "listening on" not in capture_log.read_text():
        if process.poll() is not None or time.monotonic() > deadline:
            process.kill()
            process.wait()
            raise RuntimeError("capture failed to start")
        time.sleep(0.02)
    return process


def finish_capture(process, stem):
    time.sleep(0.15)
    process.send_signal(signal.SIGINT)
    if process.wait(timeout=10):
        raise RuntimeError("capture failed")
    text = Path(str(stem) + ".capture.log").read_text()
    dropped = re.search(r"(\d+) packets dropped by kernel", text)
    if not dropped or int(dropped[1]):
        raise RuntimeError("missing/nonzero capture drops")
    result = scan.capture_summary(Path(str(stem) + ".pcap"), stem)
    if result["repeated_xids"] or result["retransmitted_rpc_frames"]:
        raise RuntimeError("unclean capture: " + str(result))
    return result


def _transfer(
    args,
    account,
    mount,
    source,
    destination,
    scenario,
    method,
    flags=(),
    status=0,
    interrupt=False,
):
    stem = args.output / (scenario + "-" + method)
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("VFSI_", "VNFS_"))
    }
    environment.update(
        LC_ALL="C",
        HOME=account.pw_dir,
        VFSI_TRACE="1",
        VFSI_IMPL="off" if method == "stock" else "nfs",
        VFSI_LIBRARY=(
            args.old_library
            if method == "old-library"
            else (
                "/nonexistent-vfsi-adoption.so"
                if method == "missing-library"
                else args.library
            )
        ),
    )
    binary = args.stock_binary if method == "stock" else args.binary
    command = [
        binary,
        "-ai",
        "--out-format=%i %n%L",
        *flags,
        source + "/",
        str(destination) + "/",
    ]
    capture = None if interrupt else start_capture(args.interface, stem)
    process = None
    try:
        with Path(str(stem) + ".stdout").open("wb") as out, Path(
            str(stem) + ".stderr"
        ).open("wb") as err:
            start = time.perf_counter_ns()
            process = subprocess.Popen(
                command,
                env=environment,
                stdout=out,
                stderr=err,
                user=account.pw_uid,
                group=account.pw_gid,
                extra_groups=os.getgrouplist(args.user, account.pw_gid),
                start_new_session=True,
            )
            if interrupt:
                deadline = time.monotonic() + 20
                while not partial_bytes(destination):
                    if process.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError(
                            "did not observe transfer progress before interruption"
                        )
                    time.sleep(0.02)
                os.killpg(process.pid, signal.SIGINT)
            actual = process.wait(timeout=20 if interrupt else args.timeout)
            elapsed = time.perf_counter_ns() - start
        if interrupt:
            if actual != 20:
                raise RuntimeError("expected rsync SIGINT exit 20, got " + str(actual))
            partial = tree_manifest(destination)
            if not any(
                value[0] == "file" and value[2] > 0 for value in partial.values()
            ):
                raise RuntimeError("interruption did not retain partial bytes")
        elif actual != status:
            raise RuntimeError(
                f"{scenario}/{method}: status {actual}, expected {status}: "
                + Path(str(stem) + ".stderr").read_text()
            )
        wire = finish_capture(capture, stem) if capture else None
        capture = None
        diagnostics = Path(str(stem) + ".stderr").read_text()
        traces = "\n".join(
            line for line in diagnostics.splitlines() if line.startswith("vfsi:")
        )
        entries = checked_trace(traces, method, partial=interrupt)
        if status == 23 and not interrupt and "Permission denied" not in diagnostics:
            raise RuntimeError("partial failure lacks native permission diagnostic")
        output = Path(str(stem) + ".stdout").read_bytes()
        rows = tree_manifest(destination)
        digest = save_manifest(str(stem) + ".tree.json.gz", rows)
        result = dict(
            scenario=scenario,
            method=method,
            command=command,
            status=actual,
            elapsed_ns=elapsed,
            traced_entries=entries,
            wire=wire,
            manifest_sha256=digest,
            entries=len(rows),
            stdout_sha256=hashlib.sha256(output).hexdigest(),
            stdout_lines=len(output.splitlines()),
        )
        for suffix in (".stdout", ".stderr"):
            path = Path(str(stem) + suffix)
            with path.open("rb") as stream, gzip.open(
                str(path) + ".gz", "wb"
            ) as target:
                shutil.copyfileobj(stream, target)
            path.unlink()
        Path(str(stem) + ".json").write_text(json.dumps(result, indent=2) + "\n")
        return rows, result
    finally:
        if process and process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        if capture:
            capture.send_signal(signal.SIGINT)
            capture.wait(timeout=10)


def transfer(
    args,
    account,
    source,
    destination,
    scenario,
    method,
    flags=(),
    status=0,
    interrupt=False,
):
    # Fresh, separate superblocks give each method the same initial client cache.
    # Source edits happened through the control NFS mount before this mount.
    with private_mount(args.export, uncached=False) as mount:
        return _transfer(
            args,
            account,
            mount,
            source,
            destination,
            scenario,
            method,
            flags,
            status=status,
            interrupt=interrupt,
        )


def mount_present(path):
    return any(
        line.split()[4] == str(path)
        for line in Path("/proc/self/mountinfo").read_text().splitlines()
    )


@contextmanager
def private_mount(export, *, uncached=True):
    root = Path(tempfile.mkdtemp(prefix="vfsi-rsync-mount-"))
    root.chmod(0o755)
    mount = root / "mount"
    mount.mkdir()
    options = "vers=4.2,proto=tcp,sec=sys,nosharecache"
    if uncached:
        options += ",actimeo=0,lookupcache=none"
    try:
        scan.checked(
            ["mount", "-t", "nfs4", "-o", options, scan.SERVER + ":" + export, mount]
        )
        # Match the frozen scan protocol: prepare CWD before capture/timing.
        # Passing cwd=mount to Popen after dropping credentials adds a kernel
        # ACCESS/GETATTR RPC and would change the 273-RPC baseline.
        previous = os.open(".", os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.chdir(mount)
            yield mount
        finally:
            os.fchdir(previous)
            os.close(previous)
    finally:
        try:
            if mount_present(mount):
                try:
                    scan.checked(["umount", mount], timeout=30)
                except BaseException:
                    # Detach only our private mount before removing its empty
                    # mountpoint; never recurse into a still-mounted source.
                    scan.checked(["umount", "-l", mount], timeout=10)
                    raise
        finally:
            if not mount_present(mount):
                shutil.rmtree(root)


def scan_worker(args):
    account = pwd.getpwnam(args.user)
    scan.checked(["mount", "--make-rprivate", "/"])
    with private_mount(args.export, uncached=False) as mount:
        with tempfile.TemporaryDirectory(prefix="vfsi-rsync-scan-") as temporary:
            root = Path(temporary)
            root.chmod(0o755)
            destination = root / "destination"
            destination.mkdir()
            os.chown(destination, account.pw_uid, account.pw_gid)
            _, row = _transfer(
                args,
                account,
                mount,
                "tree",
                destination,
                "scan",
                "stock" if args.method == "kernel" else "vfsi",
                ("--dry-run",),
            )
            if any(destination.iterdir()):
                raise RuntimeError("dry-run modified destination")
            # transfer writes scan-METHOD evidence; expose the expected row.
            Path(str(args.stem) + ".json").write_text(json.dumps(row, indent=2) + "\n")


def worker(args):
    def expire(*unused):
        raise TimeoutError("real-transfer worker exceeded its deadline")

    signal.signal(signal.SIGALRM, expire)
    signal.alarm(args.worker_timeout)
    account = pwd.getpwnam(args.user)
    scan.checked(["mount", "--make-rprivate", "/"])
    results = []
    with tempfile.TemporaryDirectory(prefix="vfsi-rsync-adoption-") as temporary:
        temp = Path(temporary)
        temp.chmod(0o755)
        with ExitStack() as mounts:
            mount = mounts.enter_context(private_mount(args.export))
            destinations = {}
            for method in ("stock", "vfsi"):
                destinations[method] = temp / method
                destinations[method].mkdir()
                os.chown(destinations[method], account.pw_uid, account.pw_gid)
            source = mount / "tree"
            host_source = args.server_root / "tree"

            def pair(name, flags=("--delete",), *, excluded=False, blocked=False):
                expected = expected_manifest(
                    host_source, excluded=excluded, blocked=blocked
                )
                for method in ("stock", "vfsi"):
                    rows, result = transfer(
                        args,
                        account,
                        "tree",
                        destinations[method],
                        name,
                        method,
                        flags,
                        status=23 if blocked else 0,
                    )
                    if rows != expected:
                        differing = [
                            key
                            for key in rows.keys() | expected.keys()
                            if rows.get(key) != expected.get(key)
                        ]
                        raise RuntimeError(
                            f"{name}/{method}: destination differs: {differing[:10]}"
                        )
                    results.append(result)

            pair("create")
            put(source / "f000-metadata-small-file", b"!" * 128, mode=0o640)
            put(source / "added.txt", b"new file\n")
            pair("update")
            (source / "f001-metadata-small-file").unlink()
            shutil.rmtree(source / "d00/d00/d00/d00")
            for destination in destinations.values():
                put(destination / "orphan.txt", b"delete me")
                (destination / "obsolete").mkdir()
                os.chown(destination / "obsolete", account.pw_uid, account.pw_gid)
                put(destination / "obsolete/child", b"delete me too")
            pair("delete")
            put(source / "skip/blocked", b"excluded directory")
            put(source / "hidden.skip", b"excluded suffix")
            put(source / "visible.txt", b"include me")
            pair(
                "excludes",
                ("--delete", "--exclude=skip/", "--exclude=*.skip"),
                excluded=True,
            )
            (source / "link-to-file").symlink_to("f000-metadata-small-file")
            (source / "dangling").symlink_to("missing-target")
            pair("symlinks")
            put(source / "blocked.bin", b"not readable", mode=0)
            put(source / "healthy-after-error.txt", b"healthy sibling")
            pair("partial-failure", blocked=True)
            (source / "blocked.bin").unlink()

            # A small representative transfer proves feature fallback without
            # conflating full-tree data throughput with metadata scan timing.
            fallback = mount / "fallback"
            put(fallback / "file", b"fallback payload")
            put(fallback / "child/data", b"nested payload")
            (fallback / "link").symlink_to("file")
            expected = tree_manifest(args.server_root / "fallback")
            for method in ("stock", "old-library", "missing-library", "unsupported"):
                destination = temp / ("fallback-" + method)
                destination.mkdir()
                os.chown(destination, account.pw_uid, account.pw_gid)
                flags = ("--hard-links",) if method == "unsupported" else ()
                rows, result = transfer(
                    args,
                    account,
                    "fallback",
                    destination,
                    "fallback",
                    method,
                    flags,
                )
                if rows != expected:
                    raise RuntimeError("fallback destination mismatch: " + method)
                results.append(result)

            put(source / "000-interrupt.bin", b"VFSI interruption\n" * (256 * 1024))
            expected = tree_manifest(host_source)
            for method in ("stock", "vfsi"):
                destination = temp / ("resume-" + method)
                destination.mkdir()
                os.chown(destination, account.pw_uid, account.pw_gid)
                _, interrupted = transfer(
                    args,
                    account,
                    "tree",
                    destination,
                    "interrupted",
                    method,
                    ("--partial", "--bwlimit=64"),
                    interrupt=True,
                )
                results.append(interrupted)
                rows, resumed = transfer(
                    args,
                    account,
                    "tree",
                    destination,
                    "resumed",
                    method,
                    ("--partial", "--delete"),
                )
                if rows != expected:
                    raise RuntimeError("resume destination mismatch: " + method)
                results.append(resumed)
    (args.output / "transfers.json").write_text(json.dumps(results, indent=2) + "\n")


def run(args):
    if not 0 < args.timeout <= 600 or not 60 <= args.worker_timeout <= 1800:
        raise ValueError("require bounded transfer and worker deadlines")
    for name in ("prepared", "fixture", "old_library", "output"):
        setattr(args, name, getattr(args, name).resolve())
    args.output.mkdir(parents=True, exist_ok=False)
    provenance = json.loads((args.prepared / "provenance.json").read_text())
    for field in ("binary", "stock_binary", "library"):
        if scan.sha(provenance[field]) != provenance[field + "_sha256"]:
            raise ValueError("prepared artifact changed: " + field)
    older = ctypes.CDLL(str(args.old_library.resolve()))
    older.vfsi_abi_version.restype = ctypes.c_uint32
    if (
        older.vfsi_abi_version() != 3
        or hasattr(older, "vfsi_preadv_ex")
        or hasattr(older, "vfsi_pwritev_ex")
    ):
        raise ValueError(
            "fallback library must be older ABI-v3 without extended outcomes"
        )
    original = scan.fingerprint(args.fixture / "tree")
    fixture = json.loads((args.fixture / "fixture.json").read_text())
    if original["identity_sha256"] != fixture["identity_sha256"]:
        raise ValueError("frozen fixture changed")
    result = dict(
        provenance=provenance,
        fixture=fixture,
        old_library_sha256=scan.sha(args.old_library),
        regimes={},
        scans=[],
        transfers=[],
    )
    owned = Path(
        tempfile.mkdtemp(prefix="vfsi-rsync-adoption-", dir=args.fixture.parent)
    )
    try:
        for regime in ("native", "added-1ms-rtt"):
            shutil.copytree(args.fixture / "tree", owned / regime / "tree")
        with scan.Link() as link:
            for regime, delay in (("native", 0), ("added-1ms-rtt", 500)):
                result["regimes"][regime] = link.delay(delay)
                raw = args.output / regime
                raw.mkdir()
                parity = None
                for method in ("kernel", "vfsi"):
                    stem = raw / ("scan-" + method)
                    command = [
                        "unshare",
                        "--mount",
                        "--fork",
                        sys.executable,
                        str(Path(__file__).resolve()),
                        "_scan",
                        "--binary",
                        provenance["stock_binary" if method == "kernel" else "binary"],
                        "--library",
                        provenance["library"],
                        "--export",
                        "/" + args.fixture.name,
                        "--method",
                        method,
                        "--user",
                        pwd.getpwuid(os.getuid()).pw_name,
                        "--output",
                        str(raw),
                        "--stem",
                        str(stem),
                        "--stock-binary",
                        provenance["stock_binary"],
                        "--old-library",
                        str(args.old_library.resolve()),
                        "--interface",
                        link.peer,
                        "--timeout",
                        str(args.timeout),
                        "--capture",
                    ]
                    link.client(*command)
                    row = json.loads(Path(str(stem) + ".json").read_text())
                    parity = scan.validate_sample(
                        row,
                        method,
                        fixture["directories"] + fixture["regular_files"],
                        parity,
                    )
                    if method == "vfsi" and row["wire"]["rpc_calls"] != 273:
                        raise RuntimeError(
                            "retained-cursor scan RPC regression: " + str(row["wire"])
                        )
                    row.update(regime=regime, method=method)
                    result["scans"].append(row)
                command = [
                    "unshare",
                    "--mount",
                    "--fork",
                    sys.executable,
                    str(Path(__file__).resolve()),
                    "_worker",
                    "--binary",
                    provenance["binary"],
                    "--stock-binary",
                    provenance["stock_binary"],
                    "--library",
                    provenance["library"],
                    "--old-library",
                    str(args.old_library.resolve()),
                    "--server-root",
                    str(owned / regime),
                    "--export",
                    "/" + owned.name + "/" + regime,
                    "--user",
                    pwd.getpwuid(os.getuid()).pw_name,
                    "--interface",
                    link.peer,
                    "--output",
                    str(raw),
                    "--timeout",
                    str(args.timeout),
                    "--worker-timeout",
                    str(args.worker_timeout),
                ]
                scan.checked(
                    ["sudo", "-n", "ip", "netns", "exec", link.ns, *command],
                    timeout=args.worker_timeout + 180,
                )
                rows = json.loads((raw / "transfers.json").read_text())
                for row in rows:
                    row["regime"] = regime
                result["transfers"].extend(rows)
                (args.output / "results.json").write_text(
                    json.dumps(result, indent=2) + "\n"
                )
        if scan.fingerprint(args.fixture / "tree") != original:
            raise RuntimeError("frozen fixture mutated")
        for field in ("binary", "stock_binary", "library"):
            if scan.sha(provenance[field]) != provenance[field + "_sha256"]:
                raise RuntimeError("prepared artifact changed during measurement")
        if scan.sha(args.old_library) != result["old_library_sha256"]:
            raise RuntimeError("fallback library changed")
        result["verified"] = True
        result["runner_sha256"] = scan.sha(Path(__file__).resolve())
        (args.output / "results.json").write_text(json.dumps(result, indent=2) + "\n")
        print(
            "All real transfers, fallback, interruption/resume, and 273-RPC gates passed",
            flush=True,
        )
    finally:
        # This path was created exclusively by this run. Never remove the source fixture.
        scan.checked(["sudo", "-n", "rm", "-rf", "--", str(owned)])


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)
    build = commands.add_parser("prepare")
    build.add_argument("--checkout", required=True, type=Path)
    build.add_argument("--output", required=True, type=Path)
    build.add_argument("--target-dir", required=True, type=Path)
    execute = commands.add_parser("run")
    for name in ("prepared", "fixture", "old-library", "output"):
        execute.add_argument("--" + name, type=Path, required=True)
    execute.add_argument("--timeout", type=float, default=600)
    execute.add_argument("--worker-timeout", type=int, default=900)
    trial = commands.add_parser("_worker")
    for name in (
        "binary",
        "stock-binary",
        "library",
        "old-library",
        "export",
        "user",
        "interface",
    ):
        trial.add_argument("--" + name, required=True)
    for name in ("server-root", "output"):
        trial.add_argument("--" + name, type=Path, required=True)
    trial.add_argument("--timeout", type=float, required=True)
    trial.add_argument("--worker-timeout", type=int, required=True)
    trial = commands.add_parser("_scan")
    for name in (
        "binary",
        "stock-binary",
        "library",
        "old-library",
        "export",
        "method",
        "user",
        "interface",
    ):
        trial.add_argument("--" + name, required=True)
    trial.add_argument("--output", type=Path, required=True)
    trial.add_argument("--stem", type=Path, required=True)
    trial.add_argument("--timeout", type=float, required=True)
    trial.add_argument("--capture", action="store_true")
    return result


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *unused: sys.exit("terminated"))
    arguments = parser().parse_args()
    {
        "prepare": prepare,
        "run": run,
        "_worker": worker,
        "_scan": scan_worker,
    }[
        arguments.command
    ](arguments)
