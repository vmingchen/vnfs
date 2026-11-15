#!/usr/bin/env python3
"""Separate syscall and wire controls for the owned integration fixtures."""

from collections import Counter
import argparse
import hashlib
import json
import os
import pwd
import subprocess
import time
from pathlib import Path


def decode_commands(text, response):
    complete, pending = Counter(), Counter()
    for line in text.splitlines():
        fields = line.split("\t")
        commands = [int(v, 0) for v in fields[0].split(",") if v]
        if response:
            statuses = (
                [int(v, 0) for v in fields[1].split(",") if v]
                if len(fields) > 1
                else []
            )
            assert len(commands) == len(statuses), "incomplete SMB status dissection"
            for command, status in zip(commands, statuses):
                (pending if status == 0x103 else complete)[command] += 1
        else:
            complete.update(commands)
    return complete, pending


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, required=True)
    w = parser.parse_args().work
    os.environ["LC_ALL"] = "C"
    capture_user = pwd.getpwuid(os.getuid()).pw_name
    out = w / "traces"
    out.mkdir(exist_ok=True)
    result = {
        "scope": "Separate traced controls; their durations are excluded from uninstrumented timing summaries.",
        "syscalls": {},
        "wire": {},
        "nfs": {},
    }
    for backend in ["posix", "uring"]:
        for scenario in ["healthy", "mixed", "all", "void"]:
            rows = {}
            for name in ["baseline", "final"]:
                stem = out / f"{backend}-{scenario}-{name}"
                summary = stem.with_suffix(".strace")
                args = [
                    "strace",
                    "-qq",
                    "-f",
                    "-c",
                    "-e",
                    "trace=pread64,pwrite64,openat,close,io_uring_enter,io_uring_setup,io_uring_register,fsync,fdatasync",
                    "-o",
                    str(summary),
                    str(w / f"{name}-timing"),
                    backend,
                    scenario,
                    "3",
                    str(w / "posix"),
                ]
                row = json.loads(subprocess.check_output(args, text=True))
                calls = {}
                for line in summary.read_text().splitlines():
                    fields = line.split()
                    if fields and fields[-1] in [
                        "pread64",
                        "pwrite64",
                        "openat",
                        "close",
                        "io_uring_enter",
                        "io_uring_setup",
                        "io_uring_register",
                        "fsync",
                        "fdatasync",
                    ]:
                        calls[fields[-1]] = int(fields[3])
                rows[name] = {
                    "counts": calls,
                    "result": row,
                    "trace_sha256": hashlib.sha256(summary.read_bytes()).hexdigest(),
                }
            assert rows["baseline"]["counts"] == rows["final"]["counts"], rows
            result["syscalls"][backend + "/" + scenario] = rows
    for backend in ["nfs41", "nfs42"]:
        for scenario in ["healthy", "mixed", "all", "lossy", "void"]:
            rows = {}
            for name in ["baseline", "final"]:
                row = json.loads(
                    subprocess.check_output(
                        [
                            str(w / f"{name}-timing"),
                            backend,
                            scenario,
                            "3",
                            (w / f"{backend}.root").read_text().strip(),
                        ],
                        text=True,
                        env=dict(os.environ, VNFS_STATS="1"),
                    )
                )
                rows[name] = row
            assert rows["baseline"]["nfs"] == rows["final"]["nfs"], rows
            result["nfs"][backend + "/" + scenario] = rows
    for scenario in ["healthy", "mixed", "all", "lossy", "void"]:
        rows = {}
        for name in ["baseline", "final"]:
            pcap = out / f"smb-{scenario}-{name}.pcap"
            log = out / f"smb-{scenario}-{name}.tcpdump"
            with log.open("w") as diagnostics:
                capture = subprocess.Popen(
                    [
                        "sudo",
                        "-n",
                        "tcpdump",
                        "-Z",
                        capture_user,
                        "-n",
                        "-B",
                        "32768",
                        "-U",
                        "-s",
                        "0",
                        "-i",
                        "lo",
                        "tcp port 445",
                        "-w",
                        str(pcap),
                    ],
                    stdout=diagnostics,
                    stderr=diagnostics,
                )
                try:
                    for _ in range(50):
                        if "listening on" in log.read_text():
                            break
                        if capture.poll() is not None:
                            raise RuntimeError(log.read_text())
                        time.sleep(0.05)
                    else:
                        raise RuntimeError("capture did not become ready")
                    row = json.loads(
                        subprocess.check_output(
                            [
                                str(w / f"{name}-timing"),
                                "smb",
                                scenario,
                                "3",
                                (w / "smb.root").read_text().strip(),
                            ],
                            text=True,
                        )
                    )
                    time.sleep(1.5)
                finally:
                    subprocess.run(
                        ["sudo", "-n", "kill", "-INT", str(capture.pid)],
                        check=False,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                    capture.wait(timeout=10)
            assert "0 packets dropped by kernel" in log.read_text(), log.read_text()
            counts = {}
            for response in ["0", "1"]:
                text = subprocess.check_output(
                    [
                        "tshark",
                        "-r",
                        str(pcap),
                        "-Y",
                        f"smb2.flags.response == {response}",
                        "-T",
                        "fields",
                        "-E",
                        "occurrence=a",
                        "-E",
                        "aggregator=,",
                        "-e",
                        "smb2.cmd",
                        "-e",
                        "smb2.nt_status",
                    ],
                    text=True,
                    stderr=subprocess.DEVNULL,
                )
                counter, pending = decode_commands(text, response == "1")
                assert counter, "empty SMB dissection"
                counts["requests" if response == "0" else "responses"] = dict(counter)
                if response == "1":
                    interim_pending = dict(pending)
            expected_reads = 0 if scenario == "void" else 64 * 23
            expected_opens = 64 + (
                64 * 23
                if scenario == "all"
                else 23 if scenario in ["mixed", "lossy"] else 0
            )
            assert counts["requests"].get(8, 0) == expected_reads, counts
            assert counts["requests"].get(5, 0) == expected_opens, counts
            assert counts["requests"].get(6, 0) == expected_opens, counts
            assert (
                counts["requests"].get(4, 0) == counts["responses"].get(4, 0) == 1
            ), counts
            rows[name] = {
                "commands": counts,
                "result": row,
                "pcap_sha256": hashlib.sha256(pcap.read_bytes()).hexdigest(),
                "interim_pending": interim_pending,
            }
        assert rows["baseline"]["commands"] == rows["final"]["commands"], (
            scenario,
            {n: r["commands"] for n, r in rows.items()},
        )
        result["wire"]["smb/" + scenario] = rows
        (w / "trace-results.json").write_text(json.dumps(result, indent=2) + "\n")
        print("SMB wire parity", scenario, rows["final"]["commands"], flush=True)
    (w / "trace-results.json").write_text(json.dumps(result, indent=2) + "\n")
    print("Syscall, NFS request/byte, and SMB wire controls passed", flush=True)


if __name__ == "__main__":
    main()
