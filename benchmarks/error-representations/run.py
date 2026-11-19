#!/usr/bin/env python3
"""Paired isolated error-envelope comparison; does not modify production APIs."""

import argparse
import hashlib
import json
import math
from pathlib import Path
import random
import shutil
import statistics
import subprocess
import tempfile
import time

SCENARIOS = [
    "construct_client",
    "construct_errno",
    "construct_nfs",
    "construct_smb",
    "construct_transport",
    "construct_context",
    "clone_errno",
    "clone_nfs",
    "clone_context",
    "inspect_nfs",
    "inspect_transport",
    "fanout_nfs",
    "fanout_context",
    "void_ok",
    "vec_ok",
    "vec_mixed_errno",
    "vec_all_errno",
    "vec_mixed_nfs",
    "vec_all_nfs",
    "posix_ok",
    "posix_mixed",
    "ring_ok",
    "ring_mixed",
]
REPRS = ["minimal", "io", "arc_io"]


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def checked(args):
    return subprocess.check_output([str(x) for x in args], text=True).strip()


def execute(binary, cpu, representation, scenario, count, fixture):
    row = json.loads(
        checked(
            ["taskset", "-c", cpu, binary, representation, scenario, count, fixture]
        )
    )
    assert row["retained_extra_bytes"] == 0
    assert row["iterations"] == count
    if scenario.startswith(("posix", "ring", "vec")):
        failures = 64 if scenario.startswith("vec_all") else int("mixed" in scenario)
        assert row["failed"] == failures * count, row
        assert row["successful"] == (64 - failures) * count, row
        assert row["returned_bytes"] == (64 - failures) * count * 4096, row
    if scenario.startswith("ring"):
        assert row["io"] == dict(sqes=64 * count, cqes=64 * count, waves=count)
    if scenario.startswith("posix"):
        assert row["io"] == dict(pread_calls=64 * count)
    return row


def interval(values):
    rng = random.Random(713)
    samples = sorted(
        statistics.median(rng.choices(values, k=len(values))) for _ in range(10000)
    )
    return [samples[249], samples[9749]]


def fingerprint(root):
    return {p.name: sha(p) for p in sorted(root.glob("file-*"))}


def main():
    if not __debug__:
        raise RuntimeError("benchmark validation assertions must be enabled")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--timing", required=True, type=Path)
    parser.add_argument("--allocations", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--cpu", default="2")
    parser.add_argument("--pairs", default=9, type=int)
    parser.add_argument("--seconds", default=0.05, type=float)
    args = parser.parse_args()
    if args.pairs < 7 or not math.isfinite(args.seconds) or args.seconds <= 0:
        parser.error("at least seven pairs and positive finite duration required")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    fixture = Path(tempfile.mkdtemp(prefix="fixture-", dir=output))
    for i in range(64):
        (fixture / f"file-{i}").write_bytes(bytes([i]) * 4096)
    (fixture / "directory").mkdir()
    before = fingerprint(fixture)
    root = Path(__file__).resolve().parents[2]
    evidence = {
        "schema": 1,
        "baseline": checked(["git", "-C", root, "rev-parse", "HEAD"]),
        "scope": "Isolated prototypes and equal-work descriptor reads, not a full vnfs backend migration.",
        "representations": REPRS,
        "pairs": args.pairs,
        "cpu": args.cpu,
        "toolchain": checked(["rustc", "-Vv"]),
        "cargo": checked(["cargo", "-vV"]),
        "uname": checked(["uname", "-a"]),
        "timing_binary_sha256": sha(args.timing),
        "allocation_binary_sha256": sha(args.allocations),
        "sources": {
            name: sha(Path(__file__).parent / name)
            for name in (
                "src/main.rs",
                "build.rs",
                "Cargo.toml",
                "Cargo.lock",
                "run.py",
            )
        },
        "layout": json.loads(checked([args.timing, "layout"])),
        "fixture": {"files": 64, "bytes_each": 4096, "hashes": before},
        "allocations": {},
        "timings": {},
        "summary": {},
    }
    rng = random.Random(20261114)
    try:
        # Separate instrumented runs: no instrumented durations become timing data.
        for scenario in SCENARIOS:
            evidence["allocations"][scenario] = {
                rep: execute(args.allocations, args.cpu, rep, scenario, 10, fixture)
                for rep in REPRS
            }
        for scenario in SCENARIOS:
            pilots = {
                rep: execute(
                    args.timing,
                    args.cpu,
                    rep,
                    scenario,
                    100 if scenario.startswith(("posix", "ring")) else 10000,
                    fixture,
                )
                for rep in REPRS
            }
            assert all(not row["allocation_instrumented"] for row in pilots.values())
            # Same iteration count for all candidates within a paired scenario.
            slowest = max(row["ns_per_iteration"] for row in pilots.values())
            count = max(100, min(2000000, int(args.seconds * 1e9 / slowest)))
            pairs = []
            for pair in range(args.pairs):
                order = REPRS.copy()
                rng.shuffle(order)
                rows = {
                    rep: execute(args.timing, args.cpu, rep, scenario, count, fixture)
                    for rep in order
                }
                assert all(not row["allocation_instrumented"] for row in rows.values())
                pairs.append({"pair": pair, "order": order, "rows": rows})
            evidence["timings"][scenario] = pairs
            medians = {
                rep: statistics.median(
                    p["rows"][rep]["ns_per_iteration"] for p in pairs
                )
                for rep in REPRS
            }
            ratios = {}
            for rep in ("io", "arc_io"):
                values = [
                    p["rows"][rep]["ns_per_iteration"]
                    / p["rows"]["minimal"]["ns_per_iteration"]
                    for p in pairs
                ]
                ratios[rep] = {
                    "median_paired_ratio": statistics.median(values),
                    "bootstrap_95_interval": interval(values),
                }
            evidence["summary"][scenario] = {
                "median_ns_per_iteration": medians,
                "ratios": ratios,
            }
            (output / "results.json").write_text(json.dumps(evidence, indent=2) + "\n")
            print(
                scenario + ": " + json.dumps(evidence["summary"][scenario]), flush=True
            )
        assert fingerprint(fixture) == before
        evidence["fixture_unchanged"] = True
        evidence["finished_unix"] = time.time()
        (output / "results.json").write_text(json.dumps(evidence, indent=2) + "\n")
    finally:
        shutil.rmtree(fixture)


if __name__ == "__main__":
    main()
