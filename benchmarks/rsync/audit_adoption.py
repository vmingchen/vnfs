#!/usr/bin/env python3
"""Recheck raw transfer manifests, process outcomes, traces and decoded RPCs."""

import argparse
import gzip
import hashlib
import json
from pathlib import Path
import re
import tempfile

import adoption

if not __debug__:
    raise RuntimeError("audit assertions must be enabled; do not use python -O")


def file_value(data, mode=0o644):
    return ["file", mode, len(data), hashlib.sha256(data).hexdigest()]


def expected_cases(fixture):
    assert (
        fixture["branches"],
        fixture["depth"],
        fixture["files_per_directory"],
        fixture["file_bytes"],
    ) == (6, 4, 8, 128)
    unit = b"VFSI du metadata benchmark\n"
    payload = (unit * 8)[:128]
    assert hashlib.sha256(payload).hexdigest() == fixture["payload_sha256"]
    source = {}

    def fill(prefix, depth):
        source[prefix or "."] = ["directory", 0o755]
        for index in range(8):
            source[prefix + f"f{index:03d}-metadata-small-file"] = file_value(payload)
        if depth < 4:
            for index in range(6):
                fill(prefix + f"d{index:02d}/", depth + 1)

    fill("", 0)
    # Directory keys match pathlib's relative paths, with no trailing slash.
    source = {key.rstrip("/") or ".": value for key, value in source.items()}
    cases = {"create": dict(source)}
    source["f000-metadata-small-file"] = file_value(b"!" * 128, 0o640)
    source["added.txt"] = file_value(b"new file\n")
    cases["update"] = dict(source)
    del source["f001-metadata-small-file"]
    source = {
        key: value
        for key, value in source.items()
        if key != "d00/d00/d00/d00" and not key.startswith("d00/d00/d00/d00/")
    }
    cases["delete"] = dict(source)
    source.update(
        {
            "skip": ["directory", 0o755],
            "skip/blocked": file_value(b"excluded directory"),
            "hidden.skip": file_value(b"excluded suffix"),
            "visible.txt": file_value(b"include me"),
        }
    )
    cases["excludes"] = {
        key: value
        for key, value in source.items()
        if key != "skip" and not key.startswith("skip/") and not key.endswith(".skip")
    }
    source["link-to-file"] = ["symlink", 0o777, "f000-metadata-small-file"]
    source["dangling"] = ["symlink", 0o777, "missing-target"]
    cases["symlinks"] = dict(source)
    source["healthy-after-error.txt"] = file_value(b"healthy sibling")
    cases["partial-failure"] = dict(source)
    source["000-interrupt.bin"] = file_value(b"VFSI interruption\n" * (256 * 1024))
    cases["resumed"] = dict(source)
    cases["fallback"] = {
        ".": ["directory", 0o755],
        "file": file_value(b"fallback payload"),
        "child": ["directory", 0o755],
        "child/data": file_value(b"nested payload"),
        "link": ["symlink", 0o777, "file"],
    }
    return cases


def audit(root, fixture_path, output):
    if output.exists():
        raise FileExistsError("audit output must be a new file: " + str(output))
    result = json.loads((root / "results.json").read_text())
    assert result["verified"] is True
    assert adoption.scan.sha(root / "runner-snapshot.py") == result["runner_sha256"]
    original = adoption.scan.fingerprint(fixture_path / "tree")
    assert original["identity_sha256"] == result["fixture"]["identity_sha256"]
    cases = expected_cases(result["fixture"])
    expected = set()
    for regime in ("native", "added-1ms-rtt"):
        for scenario in (
            "create",
            "update",
            "delete",
            "excludes",
            "symlinks",
            "partial-failure",
            "interrupted",
            "resumed",
        ):
            expected.update((regime, scenario, method) for method in ("stock", "vfsi"))
        expected.update(
            (regime, "fallback", method)
            for method in ("stock", "old-library", "missing-library", "unsupported")
        )
    seen = set()
    decoded = 0
    for row in result["transfers"]:
        key = row["regime"], row["scenario"], row["method"]
        assert key not in seen
        seen.add(key)
        stem = root / row["regime"] / (row["scenario"] + "-" + row["method"])
        raw = json.loads(Path(str(stem) + ".json").read_text())
        assert raw == {key: value for key, value in row.items() if key != "regime"}
        contents = gzip.open(str(stem) + ".tree.json.gz", "rb").read()
        assert hashlib.sha256(contents).hexdigest() == row["manifest_sha256"]
        rows = json.loads(contents)
        assert len(rows) == row["entries"]
        expected_status = (
            20
            if row["scenario"] == "interrupted"
            else 23 if row["scenario"] == "partial-failure" else 0
        )
        assert row["status"] == expected_status
        if row["scenario"] == "interrupted":
            partial = rows["000-interrupt.bin"]
            assert (
                partial[0] == "file"
                and 0 < partial[2] < cases["resumed"]["000-interrupt.bin"][2]
            )
            pattern = b"VFSI interruption\n"
            prefix = (pattern * (partial[2] // len(pattern) + 1))[: partial[2]]
            assert partial[3] == hashlib.sha256(prefix).hexdigest()
            assert row["wire"] is None
        else:
            assert rows == cases[row["scenario"]], (
                key,
                [
                    k
                    for k in rows.keys() | cases[row["scenario"]].keys()
                    if rows.get(k) != cases[row["scenario"]].get(k)
                ][:8],
            )
        stderr = gzip.open(str(stem) + ".stderr.gz", "rt").read()
        trace = "\n".join(
            line for line in stderr.splitlines() if line.startswith("vfsi:")
        )
        assert (
            adoption.checked_trace(trace, row["method"], partial=expected_status == 20)
            == row["traced_entries"]
        )
        if expected_status == 23:
            assert "Permission denied" in stderr
        stdout = gzip.open(str(stem) + ".stdout.gz", "rb").read()
        assert hashlib.sha256(stdout).hexdigest() == row["stdout_sha256"]
        assert len(stdout.splitlines()) == row["stdout_lines"]
        if row["wire"]:
            recheck_wire(stem, row["wire"])
            decoded += 1
    assert seen == expected, seen ^ expected
    assert len(result["transfers"]) == 40
    scans = set()
    for regime in ("native", "added-1ms-rtt"):
        parity = None
        for method in ("kernel", "vfsi"):
            row = next(
                row
                for row in result["scans"]
                if (row["regime"], row["method"]) == (regime, method)
            )
            scans.add((regime, method))
            stem = root / regime / ("scan-stock" if method == "kernel" else "scan-vfsi")
            stdout = gzip.open(str(stem) + ".stdout.gz", "rb").read()
            assert hashlib.sha256(stdout).hexdigest() == row["stdout_sha256"]
            assert (
                len(stdout.splitlines())
                == result["fixture"]["directories"] + result["fixture"]["regular_files"]
            )
            if parity is None:
                parity = stdout
            assert stdout == parity
            stderr = gzip.open(str(stem) + ".stderr.gz", "rt").read()
            assert adoption.checked_trace(stderr, method) == row["traced_entries"]
            recheck_wire(stem, row["wire"])
            decoded += 1
            if method == "vfsi":
                assert row["wire"]["rpc_calls"] == 273
    assert len(result["scans"]) == len(scans) == 4
    assert adoption.scan.fingerprint(fixture_path / "tree") == original
    proof = dict(
        verified=True,
        transfer_trials=40,
        scan_trials=4,
        independently_decoded_captures=decoded,
        destination_recipes_verified=True,
        source_identity_unchanged=original["identity_sha256"],
        results_sha256=adoption.scan.sha(root / "results.json"),
    )
    with output.open("x") as stream:
        stream.write(json.dumps(proof, indent=2) + "\n")
    print(json.dumps(proof, indent=2), flush=True)


def recheck_wire(stem, expected):
    text = Path(str(stem) + ".capture.log").read_text()
    drops = re.search(r"(\d+) packets dropped by kernel", text)
    assert drops and int(drops[1]) == 0
    with tempfile.TemporaryDirectory(prefix="vfsi-rsync-audit-") as temp:
        path = Path(temp) / "capture.pcap"
        with gzip.open(str(stem) + ".pcap.gz", "rb") as source, path.open(
            "wb"
        ) as target:
            import shutil

            shutil.copyfileobj(source, target)
        assert adoption.scan.sha(path) == expected["pcap_sha256"]
        actual = adoption.scan.capture_summary(path, Path(temp) / "capture")
        assert actual == expected


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    audit(args.results, args.fixture, args.output)
