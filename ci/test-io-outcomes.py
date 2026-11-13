#!/usr/bin/env python3
"""Run the standalone C consumer through the real-server outcome fixture."""

import argparse
import hashlib
import json
from pathlib import Path
import os
import select
import sys
import subprocess
import tempfile

from nfs_outcome_fixture import NfsOutcomeProxy, SCENARIO_OUTCOMES


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--consumer", required=True)
    parser.add_argument("--library")
    parser.add_argument("--adapter", choices=("c", "rust", "python"), default="c")
    parser.add_argument("--server", default="127.0.0.1:2049")
    parser.add_argument("--minor", required=True, choices=("1", "2"))
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    records = []
    if args.output and (args.output.exists() or not args.output.parent.is_dir()):
        parser.error("--output must be a new file in an existing directory")
    if args.adapter == "c" and not args.library:
        parser.error("--library is required for C")
    host, port = args.server.rsplit(":", 1)
    if host.startswith("[") and host.endswith("]"):
        host = host[1:-1]
    for mode, expected_outcomes in SCENARIO_OUTCOMES.items():
        proxy = NfsOutcomeProxy((host, int(port)))
        env = dict(
            os.environ,
            VFSI_OUTCOME_FAULT=mode,
            VFSI_OUTCOME_VERIFY_SERVER=args.server,
            VFSI_OUTCOME_MINOR=args.minor,
        )
        with tempfile.TemporaryFile(mode="w+") as errors:
            command = (
                [args.consumer, args.library, "nfs", proxy.endpoint, args.minor]
                if args.adapter == "c"
                else ([sys.executable] if args.adapter == "python" else [])
                + [args.consumer, proxy.endpoint, args.minor]
            )
            child = subprocess.Popen(
                command,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=errors,
                text=True,
                env=env,
            )
            try:
                assert select.select([child.stdout], [], [], 20)[
                    0
                ], "consumer readiness timeout"
                ready = child.stdout.readline().strip()
                if ready != "READY":
                    errors.seek(0)
                    raise AssertionError(
                        ("consumer failed before arming", ready, errors.read())
                    )
                proxy.arm(mode)
                output, _ = child.communicate("\n", timeout=45)
                errors.seek(0)
                assert child.returncode == 0, (
                    mode,
                    output,
                    errors.read(),
                    proxy.writes,
                )
                lines = output.strip().splitlines()
                assert len(lines) == 2 and lines[0].startswith("OUTCOMES "), output
                assert (
                    lines[1] == args.adapter.capitalize() + " outcome consumer: PASS"
                ), output
                actual = [
                    (field.split(":")[0], int(field.split(":")[1]))
                    for field in lines[0].split()[1:]
                ]
                assert actual == expected_outcomes, (mode, actual, expected_outcomes)
                assert len(proxy.writes) == (
                    2 if mode in ("short", "prefix", "uncached-later") else 1
                ), proxy.writes
                if mode in ("later", "uncached-later"):
                    assert proxy.writes[0][0]["offset"] == 0, proxy.writes
                    assert (
                        0
                        < sum(op["bytes"] for op in proxy.writes[0])
                        < 16 * 1024 * 1024
                    ), proxy.writes
                    assert all(
                        op["offset"] < 16 * 1024 * 1024 for op in proxy.writes[0]
                    ), proxy.writes
                else:
                    assert proxy.writes[0] == [{"offset": 0, "bytes": 16}], proxy.writes
                if mode == "uncached-later":
                    assert proxy.writes[1] == [
                        {"offset": 32 * 1024 * 1024, "bytes": 1}
                    ], proxy.writes
                elif len(proxy.writes) == 2:
                    assert proxy.writes[1] == [{"offset": 4, "bytes": 12}], proxy.writes
                records.append(
                    dict(scenario=mode, outcomes=actual, writes=proxy.writes)
                )
                print(
                    "%s NFSv4.%s %s: PASS (%s)"
                    % (args.adapter, args.minor, mode, proxy.writes),
                    flush=True,
                )
            finally:
                if child.poll() is None:
                    child.kill()
                child.communicate()
                proxy.close()

    if args.output:
        digest = lambda path: hashlib.sha256(Path(path).read_bytes()).hexdigest()
        evidence = dict(
            adapter=args.adapter,
            server=args.server,
            minor=args.minor,
            consumer_sha256=digest(args.consumer),
            library_sha256=digest(args.library) if args.library else None,
            fixture_sha256=digest(Path(__file__).with_name("nfs_outcome_fixture.py")),
            records=records,
        )
        with args.output.open("x") as stream:
            json.dump(evidence, stream, indent=2)
            stream.write("\n")


if __name__ == "__main__":
    main()
