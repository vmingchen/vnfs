#!/usr/bin/env python3
"""Development rsync scan comparison using explicit source revisions and an optional SDK patch.

Shares measurement, isolated networking, fallback, and packet capture with the
frozen runner. The frozen CLI and its published-source checks are unchanged.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tarfile

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("rsync_scan", HERE / "run.py")
scan = importlib.util.module_from_spec(spec)
spec.loader.exec_module(scan)


def checked_trace(text, method):
    entries, retained, modes = 0, 0, []
    for line in text.splitlines():
        match = re.fullmatch(
            r"vfsi: listdirs completed mode=(cohort|paths) dirs=(\d+) cursors=(\d+) entries=(\d+)",
            line,
        )
        if match:
            mode, directories, cursors, count = match.groups()
            if not 1 <= int(directories) <= 32 or int(cursors) > int(directories):
                raise ValueError("invalid directory cohort accounting")
            entries += int(count)
            retained += int(cursors)
            modes.append(mode)
        elif line and method == "vfsi":
            raise ValueError("unexpected diagnostics: possible partial POSIX fallback")
    if (method == "vfsi") != (entries > 0):
        raise ValueError("acceleration/fallback trace contract failed")
    if method == "vfsi" and (not modes or set(modes) != {"cohort"}):
        raise ValueError("development run must use retained-cohort symbols")
    if method == "vfsi" and retained == 0:
        raise ValueError("development run must reuse a retained cursor")
    return entries


def snapshot(checkout, revision, output, label, patch=None):
    """Archive an exact commit without modifying the input worktree or its refs."""
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError(f"{label} revision must be a full commit hash")
    resolved = scan.checked(["git", "-C", checkout, "rev-parse", revision + "^{commit}"]).strip()
    if resolved != revision:
        raise ValueError(f"{label} revision did not resolve exactly")
    checkout, output = Path(checkout).resolve(), Path(output).resolve()
    archive = output / (label + ".tar")
    with archive.open("wb") as stream:
        subprocess.run(["git", "-C", str(checkout), "archive", "--format=tar", revision],
                       stdout=stream, check=True, timeout=300)
    source = output / (label + "-source")
    source.mkdir()
    with tarfile.open(archive) as tar:
        tar.extractall(source, filter="data")
    evidence = dict(revision=revision, archive_sha256=scan.sha(archive))
    if patch:
        saved = output / (label + ".patch")
        shutil.copyfile(patch, saved)
        evidence["patch_sha256"] = scan.sha(saved)
        # Apply to a plain tree even when output is inside another worktree.
        # Inherited Git context must not redirect discovery or patch targets.
        env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        env["GIT_CEILING_DIRECTORIES"] = str(source.parent)
        subprocess.run(["git", "-C", str(source), "apply", "--check", str(saved)], env=env, check=True)
        subprocess.run(["git", "-C", str(source), "apply", str(saved)], env=env, check=True)
    return source, evidence


def build(args, output):
    output = output.resolve()
    sdk_source, sdk_evidence = snapshot(args.sdk_checkout, args.sdk_revision, output, "sdk", args.sdk_patch)
    port_source, port_evidence = snapshot(args.checkout, args.port_revision, output, "port")
    prefix = output / "sdk"
    target = args.target_dir.resolve()
    with (output / "build.log").open("w") as log:
        subprocess.run([
            sys.executable, str(sdk_source / "bindings/c/build-sdk.py"),
            "--profile", "release", "--target-dir", str(target), "--prefix", str(prefix),
        ], cwd=sdk_source, stdout=log, stderr=log, check=True, timeout=1800)
        env = dict(os.environ, CPPFLAGS="-I" + str(prefix / "include"))
        subprocess.run([
            "./configure", "--enable-vfsi", "--disable-md2man", "--disable-xxhash",
            "--disable-zstd", "--disable-lz4", "--disable-idn",
        ], cwd=port_source, env=env, stdout=log, stderr=log, check=True, timeout=300)
        subprocess.run(["make", "-j2"], cwd=port_source, stdout=log, stderr=log,
                       check=True, timeout=1200)
    binary = port_source / "rsync"
    library = prefix / "lib/libvfsi_c.so"
    provenance = dict(
        mode="development", sdk=sdk_evidence, port=port_evidence,
        binary_sha256=scan.sha(binary), library_sha256=scan.sha(library),
        sdk_lock_sha256=scan.sha(sdk_source / "Cargo.lock"),
        versions=json.loads((prefix / "versions.json").read_text()),
        rustc=scan.checked(["rustc", "-Vv"]),
        compiler=scan.checked(["cc", "--version"]).splitlines()[0],
        build_environment={k: v for k, v in os.environ.items() if k in {
            "CARGO_BUILD_JOBS", "CARGO_NET_OFFLINE", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS",
            "CC", "CFLAGS", "CARGO_BUILD_TARGET",
        } or k.startswith("CARGO_PROFILE_RELEASE_")},
        measurement_sha256={str(p): scan.sha(p) for p in [
            Path(__file__).resolve(), HERE / "run.py", HERE.parent / "du/run.py",
            HERE.parent / "du/generate.py",
        ]},
    )
    return binary, library, provenance


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run")
    for name in ("checkout", "sdk-checkout", "target-dir", "fixture", "output"):
        run.add_argument("--" + name, type=Path, required=True)
    for name in ("port-revision", "sdk-revision", "export"):
        run.add_argument("--" + name, required=True)
    run.add_argument("--sdk-patch", type=Path)
    run.add_argument("--pairs", type=int, default=7)
    run.add_argument("--captures", type=int, default=3)
    run.add_argument("--timeout", type=float, default=120)
    trial = commands.add_parser("_trial")
    for name in ("binary", "library", "export", "method", "user", "interface"):
        trial.add_argument("--" + name, required=True)
    trial.add_argument("--output", type=Path, required=True)
    trial.add_argument("--capture", action="store_true")
    trial.add_argument("--timeout", type=float, required=True)
    return result


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *unused: sys.exit("terminated"))
    args = parser().parse_args()
    if args.command == "_trial":
        scan.worker(args, trace_check=checked_trace)
    else:
        scan.main(args, prepare=lambda checkout, output: build(args, output),
                  trial_script=Path(__file__).resolve())
