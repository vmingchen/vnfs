"""Live Samba fixtures for vsmbfs."""

import os
import uuid

import fsspec
import pytest
import vsmbfs  # noqa: F401  (registers the "vsmbfs" protocol)


@pytest.fixture
def smb_fs():
    server = os.environ.get("VFSI_SMB_SERVER")
    share = os.environ.get("VFSI_SMB_SHARE")
    if not server or not share:
        if os.environ.get("VFSI_SMB_REQUIRED") == "1":
            pytest.fail("required SMB server/share is not configured")
        pytest.skip("SMB integration server is not configured")
    root = f"vsmbfs-it-{os.getpid()}-{uuid.uuid4().hex[:8]}"
    fs = fsspec.filesystem(
        "vsmbfs",
        host=server,
        share=share,
        username=os.environ.get("VFSI_SMB_USERNAME", ""),
        password=os.environ.get("VFSI_SMB_PASSWORD", ""),
        domain=os.environ.get("VFSI_SMB_DOMAIN", ""),
        root=root,
        auto_mkdir=True,
        skip_instance_cache=True,
    )
    fs.mkdir("vsmbfs:///", create_parents=True)
    yield fs
    try:
        fs.rm("vsmbfs:///", recursive=True)
    except Exception:
        pass
    fs.close()
