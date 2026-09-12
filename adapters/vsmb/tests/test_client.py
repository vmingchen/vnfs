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
        client.ensure_dir(root, 0o755)
        paths = [f"{root}/a", f"{root}/b"]
        try:
            assert client.write_many(paths, [b"alpha", b"beta"]) == [5, 4]
            values, errors = client.read_all_many(paths)
            assert errors == {}
            assert values == [b"alpha", b"beta"]
        finally:
            client.rm([root], True)
