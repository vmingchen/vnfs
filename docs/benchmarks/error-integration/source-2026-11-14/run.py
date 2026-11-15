#!/usr/bin/env python3
"""Randomized paired actual-Vfsi comparison. Timing and allocation binaries differ."""

import argparse, hashlib, json, os, random, statistics, subprocess
from pathlib import Path


def checked(args, **kwargs):
    return subprocess.check_output([str(a) for a in args], text=True, **kwargs).strip()


def sha(p):
    return hashlib.sha256(Path(p).read_bytes()).hexdigest()


def interval(values):
    rng = random.Random(20261114)
    samples = sorted(
        statistics.median(rng.choices(values, k=len(values))) for _ in range(10000)
    )
    return [samples[249], samples[9749]]


def parity(rows):
    fields = [
        "facts",
        "selected_error",
        "successes_per_iteration",
        "failures_per_iteration",
        "closed_handles",
        "ring",
        "nfs",
        "digest",
        "released_sources",
    ]
    for f in fields:
        assert rows["baseline"].get(f) == rows["final"].get(f), (f, rows)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--work", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--pairs", type=int, default=9)
    p.add_argument("--cpu", default="2")
    p.add_argument("--controls-only", action="store_true")
    a = p.parse_args()
    if a.pairs < 7:
        p.error("at least seven pairs required")
    if not __debug__:
        raise RuntimeError("assertions must be enabled")
    w = a.work
    roots = {
        "posix": w / "posix",
        "uring": w / "posix",
        **{b: (w / f"{b}.root").read_text().strip() for b in ["nfs41", "nfs42", "smb"]},
    }
    binaries = {
        name: {mode: w / f"{name}-{mode}" for mode in ["timing", "allocations"]}
        for name in ["baseline", "final"]
    }
    result = {
        "schema": 1,
        "pairs": a.pairs,
        "cpu": a.cpu,
        "scope": "Actual Vfsi clients; equal VecResult semantics; native error boxing accepted, no sequential baseline.",
        "rustc": checked(["rustc", "-Vv"]),
        "cargo": checked(["cargo", "-Vv"]),
        "uname": checked(["uname", "-a"]),
        "revisions": {n: (w / f"{n}.revision").read_text().strip() for n in binaries},
        "binary_sha256": {
            n: {m: sha(b) for m, b in rows.items()} for n, rows in binaries.items()
        },
        "sources": {
            n: {
                f: sha(w / f"{n}-source/benchmarks/error-integration" / f)
                for f in ["Cargo.toml", "Cargo.lock", "src/main.rs"]
            }
            for n in binaries
        },
        "final_source_delta_sha256": sha(w / "final-source.patch"),
        "production_source_sha256": {
            n: sha(w / f"{n}-source/crates/vfsi-nfs/src/nfs/io.rs") for n in binaries
        },
        "fixture": {b: str(r) for b, r in roots.items()},
        "allocation_controls": {},
        "timings": {},
        "summary": {},
    }

    def execute(name, mode, b, s, count):
        row = json.loads(
            checked(
                ["taskset", "-c", a.cpu, binaries[name][mode], b, s, count, roots[b]],
                timeout=180,
            )
        )
        assert row["iterations"] == count and row["allocation_instrumented"] == (
            mode == "allocations"
        ), row
        if not s.startswith("fanout_"):
            assert row["closed_handles"] == 64, row
            if b == "uring":
                assert row["ring"]["sqes"] == row["ring"]["cqes"] == 64 * count, row
                assert row["ring"]["waves"] == count, row
                assert (
                    row["ring"]["retained_descriptors"]
                    == row["ring"]["retained_buffer_bytes"]
                    == 0
                ), row
            if b.startswith("nfs") and s != "void":
                assert row["nfs"]["rpcs"] > 0, row
        return row

    cases = [
        (b, s) for b in roots for s in ["healthy", "mixed", "all", "lossy", "void"]
    ] + [("posix", s) for s in ["fanout_nfs", "fanout_opaque"]]
    rng = random.Random(20261114)
    rng.shuffle(cases)
    # Allocation controls are separate from timing; preserve peak-live and retained deltas.
    for b, s in cases:
        key = f"{b}/{s}"
        rows = {n: execute(n, "allocations", b, s, 10) for n in binaries}
        parity(rows)
        result["allocation_controls"][key] = rows
    if a.controls_only:
        a.output.write_text(json.dumps(result, indent=2) + "\n")
        return
    for b, s in cases:
        key = f"{b}/{s}"
        pilots = {n: execute(n, "timing", b, s, 1) for n in binaries}
        parity(pilots)
        count = max(
            1,
            min(
                100000,
                int(0.12e9 / max(r["ns_per_iteration"] for r in pilots.values())),
            ),
        )
        pairs = []
        for i in range(a.pairs):
            order = list(binaries)
            rng.shuffle(order)
            rows = {n: execute(n, "timing", b, s, count) for n in order}
            parity(rows)
            pairs.append({"pair": i, "order": order, "rows": rows})
        ratios = [
            pair["rows"]["final"]["ns_per_iteration"]
            / pair["rows"]["baseline"]["ns_per_iteration"]
            for pair in pairs
        ]
        result["timings"][key] = pairs
        result["summary"][key] = {
            "iterations": count,
            "baseline_ns": statistics.median(
                r["rows"]["baseline"]["ns_per_iteration"] for r in pairs
            ),
            "final_ns": statistics.median(
                r["rows"]["final"]["ns_per_iteration"] for r in pairs
            ),
            "paired_ratio_median": statistics.median(ratios),
            "paired_ratio_median_bootstrap_95": interval(ratios),
        }
        a.output.write_text(json.dumps(result, indent=2) + "\n")
        print(key, json.dumps(result["summary"][key]), flush=True)
    a.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
