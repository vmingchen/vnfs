"""Public API and packaging boundary tests for vsmb."""

import importlib.metadata
import os
import uuid

import pytest
import vsmb


def test_distribution_has_no_fsspec_dependency():
    requires = importlib.metadata.requires("vsmb") or []
    assert not any(requirement.lower().startswith("fsspec") for requirement in requires)


def test_client_requires_nonempty_share():
    with pytest.raises(ValueError, match="share must not be empty"):
        vsmb.SmbClient("127.0.0.1", "")


def test_client_forces_smb_backend(monkeypatch):
    calls = []

    class NativeClient:
        def __init__(self, **kwargs):
            calls.append(kwargs)

        def shutdown(self):
            calls.append("shutdown")

        def stat_many(self, paths):
            return paths

    monkeypatch.setattr(vsmb._native, "NfsClient", NativeClient)
    with vsmb.SmbClient("server", "share", "user", "secret") as client:
        assert client.stat_many(["/a", "/b"]) == ["/a", "/b"]

    assert calls[0]["host"] == "server"
    assert calls[0]["backend"] == "smb"
    assert calls[0]["share"] == "share"
    assert calls[0]["username"] == "user"
    assert calls[0]["password"] == "secret"
    assert calls[1] == "shutdown"


def test_live_vector_client_round_trip():
    server = os.environ.get("VFSI_SMB_SERVER")
    share = os.environ.get("VFSI_SMB_SHARE")
    if not server or not share:
        if os.environ.get("VFSI_SMB_REQUIRED") == "1":
            pytest.fail("required SMB server/share is not configured")
        pytest.skip("SMB integration server is not configured")
    root = f"/vsmb-client-{os.getpid()}-{uuid.uuid4().hex[:8]}"
    with vsmb.SmbClient(
        server,
        share,
        username=os.environ.get("VFSI_SMB_USERNAME", ""),
        password=os.environ.get("VFSI_SMB_PASSWORD", ""),
        domain=os.environ.get("VFSI_SMB_DOMAIN", ""),
    ) as client:
        client.ensure_dir(root, 0o777)
        paths = [f"{root}/a", f"{root}/b"]
        try:
            # Implicit scalar/grouped opens use share permissions. Explicit
            # restrictive permissions must still fail, never be ignored.
            for path in paths:
                with pytest.raises(NotImplementedError):
                    client.open(path, "wb", permissions=0o644)
                assert not client.exists(path)
                fd = client.open(path, "wb")
                client.close_many([fd])
            fds = client.open_many(paths, ["wb", "wb"])
            client.close_many(fds)
            assert client.write_many(paths, [b"alpha", b"beta"]) == [5, 4]
            values, errors = client.read_all_many(paths)
            assert errors == {}
            assert values == [b"alpha", b"beta"]
        finally:
            # SMB's scalar recursive rm is intentionally unsupported. Remove
            # the two known children before removing their empty directory.
            client.remove_many(paths)
            client.remove_many([root])


def test_live_copy_rejects_case_alias_and_copies_distinct_siblings():
    server = os.environ.get("VFSI_SMB_SERVER")
    share = os.environ.get("VFSI_SMB_SHARE")
    if not server or not share:
        if os.environ.get("VFSI_SMB_REQUIRED") == "1":
            pytest.fail("required SMB server/share is not configured")
        pytest.skip("SMB integration server is not configured")
    root = f"/vsmb-copy-{os.getpid()}-{uuid.uuid4().hex[:8]}"
    with vsmb.SmbClient(
        server,
        share,
        username=os.environ.get("VFSI_SMB_USERNAME", ""),
        password=os.environ.get("VFSI_SMB_PASSWORD", ""),
        domain=os.environ.get("VFSI_SMB_DOMAIN", ""),
    ) as client:
        client.ensure_dir(root, 0o777)
        source = f"{root}/MiXeD"
        alias = f"{root}/mixed"
        existing = f"{root}/existing"
        created = f"{root}/created"
        try:
            assert client.write_many([source, existing], [b"alpha", b"stale-tail"]) == [
                5,
                10,
            ]
            # Confirm the fixture's ordinary case-insensitive lookup semantics.
            assert client.read_all_many([alias]) == ([b"alpha"], {})
            copied, errors = client.copy_many([(source, alias)])
            assert copied == [None]
            assert set(errors) == {0}
            assert "same file" in str(errors[0])

            copied, errors = client.copy_many(
                [(source, existing), (source, alias), (source, created)]
            )
            assert copied == [5, None, 5]
            assert set(errors) == {1}
            assert "same file" in str(errors[1])
            assert errors[1].index == 1
            assert client.read_all_many([source, existing, created]) == (
                [b"alpha", b"alpha", b"alpha"],
                {},
            )

            # All small sources were read before any of the writes, including
            # when a destination in this batch is also a later source.
            assert client.write_many([existing], [b"original b"]) == [10]
            assert client.copy_many([(source, existing), (existing, created)]) == (
                [5, 10],
                {},
            )
            assert client.read_all_many([existing, created]) == (
                [b"alpha", b"original b"],
                {},
            )
        finally:
            for path in [source, existing, created]:
                if client.exists(path):
                    client.remove_many([path])
            client.remove_many([root])
