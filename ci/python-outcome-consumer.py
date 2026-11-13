#!/usr/bin/env python3
"""Native Python consumer of the same wire fixture as public Rust and C."""

import errno
import os
import sys

from nfs4fs._native import NfsClient


def main():
    host, minor = sys.argv[1], int(sys.argv[2])
    mode = os.environ["VFSI_OUTCOME_FAULT"]
    large = mode in ("later", "uncached-later")
    independent = mode == "uncached-later"
    path = "/vfsi-python-outcomes-%s" % os.getpid()
    client = NfsClient(host, minor_version=minor, auth="auth_sys")
    fd = client.open(path, "xb+")
    data = b"a" * (16 * 1024 * 1024) if large else b"abcdefghijklmnop"
    fds, offsets, payloads = [fd], [0], [data]
    if large:
        fds.append(fd)
        offsets.append(32 * 1024 * 1024)
        payloads.append(b"b")
    fds.append(-1)
    offsets.append(0)
    payloads.append(b"bad")
    print("READY", flush=True)
    assert sys.stdin.read(1) == "\n"
    outcomes = client.pwritev(fds, offsets, payloads)
    assert len(outcomes) == len(fds)
    if mode == "short":
        assert outcomes[0] == 16
    else:
        error = outcomes[0]
        assert isinstance(error, OSError), error
        assert error.failure_state == ("failed" if mode == "prefix" else "unknown")
        assert getattr(error, "bytes_written", 0) == (4 if mode == "prefix" else 0)
        if mode.startswith("uncached") or mode == "prefix":
            assert error.status_domain == "nfs"
            assert error.status_code == (28 if mode == "prefix" else 10068)
        else:
            assert error.transport_kind in ("connection", "timeout", "other")
    if large:
        if independent:
            assert outcomes[1] == 1
        else:
            error = outcomes[1]
            assert isinstance(error, OSError) and error.failure_state == "not_attempted"
            assert getattr(error, "bytes_written", 0) == 0
            assert error.transport_kind == outcomes[0].transport_kind
    rejected = outcomes[-1]
    assert isinstance(rejected, OSError)
    assert rejected.failure_state == "not_attempted" and rejected.errno == errno.EBADF
    assert getattr(rejected, "bytes_written", 0) == 0
    print(
        "OUTCOMES",
        *(
            (
                ("success:%s" % result)
                if isinstance(result, int)
                else "%s:%s"
                % (result.failure_state, getattr(result, "bytes_written", 0))
            )
            for result in outcomes
        ),
        flush=True,
    )
    client.shutdown()

    fresh = NfsClient(
        os.environ["VFSI_OUTCOME_VERIFY_SERVER"], minor_version=minor, auth="auth_sys"
    )
    reader = fresh.open(path, "rb")
    expected = (
        b"a" * 32 if large else b"abcd" if mode == "prefix" else b"abcdefghijklmnop"
    )
    assert fresh.pread(reader, 32, 0) == expected
    if independent:
        assert fresh.pread(reader, 1, 32 * 1024 * 1024) == b"b"
    fresh.close(reader)
    try:
        fresh.pread(reader, 1, 0)
    except OSError as error:
        assert error.errno == errno.EBADF
    else:
        raise AssertionError("closed descriptor remained usable")
    fresh.remove_many([path])
    fresh.shutdown()
    print("Python outcome consumer: PASS", flush=True)


if __name__ == "__main__":
    main()
