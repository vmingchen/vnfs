"""Mount discovery remains one direct NFS configuration across the instance."""

import errno
import os
import uuid
from pathlib import Path
from types import SimpleNamespace

import fsspec
import pytest
from nfs4fs import Nfs4FileSystem, _native


@pytest.mark.parametrize("mount", ["", ".", "data", "../data", Path("data")])
def test_relative_mount_is_rejected_before_discovery_after_chdir(
    monkeypatch, tmp_path, mount
):
    def unexpected_discovery(path):
        pytest.fail("relative mounts must fail before discovery or instance caching")

    monkeypatch.setattr(_native, "discover_mount", unexpected_discovery)
    for directory in [tmp_path, tmp_path.parent]:
        monkeypatch.chdir(directory)
        # Leave fsspec's instance cache enabled to cover the wrong-root bug.
        with pytest.raises(ValueError, match="mount.*absolute"):
            fsspec.filesystem("nfs4", mount=mount)


@pytest.mark.parametrize(
    "options",
    [
        {"host": "server"},
        {"root": "export"},
        {"auth": "auth_sys"},
        {"minor_version": 2},
        {"backend": "dummy"},
        {"service_principal": "nfs@server"},
    ],
)
def test_mount_rejects_connection_overrides_before_discovery(monkeypatch, options):
    def unexpected_discovery(path):
        pytest.fail("conflicting options must fail before discovery or network I/O")

    monkeypatch.setattr(_native, "discover_mount", unexpected_discovery)
    with pytest.raises(ValueError, match="mount="):
        Nfs4FileSystem(mount="/mnt/nfs", skip_instance_cache=True, **options)


def test_mount_configuration_is_reused_by_pool_and_reconnect(monkeypatch):
    config = SimpleNamespace(
        host="192.0.2.7:2049",
        root="/export/git/tree",
        local_path="/mnt/nfs/git/tree",
        minor_version=2,
        read_only=True,
    )
    discoveries = []
    connections = []

    def discover(path):
        discoveries.append(path)
        return config

    class Native:
        def read_all_many(self, paths, max_total_bytes=None):
            pytest.fail("mount configuration must not read file contents")

        def __init__(self, *args):
            connections.append(args)

        def shutdown(self):
            pass

        def chdir(self, path):
            pass

    monkeypatch.setattr(_native, "discover_mount", discover)
    monkeypatch.setattr(_native, "NfsClient", Native)
    with Nfs4FileSystem(
        mount=Path(config.local_path),
        connection_pool_size=2,
        request_timeout=1.5,
        skip_instance_cache=True,
    ) as fs:
        assert fs.mount == config.local_path
        assert fs.read_only
        assert fs.host == config.host
        assert fs.auth == "auth_sys"
        assert fs._native_path("/file-1") == "/file-1"
        fs._client.reconnect()
    assert discoveries == [config.local_path]
    assert len(connections) == 4
    assert all(args[-1] is config for args in connections)
    assert all(args[4] == 2 and args[10] == 1.5 for args in connections)


def test_native_discovery_rejects_local_directory(tmp_path):
    if not os.path.exists("/proc/self/mountinfo"):
        pytest.skip("Linux mount discovery")
    with pytest.raises(OSError, match="sec=sys") as error:
        fsspec.filesystem("nfs4", mount=str(tmp_path), skip_instance_cache=True)
    assert error.value.errno == errno.EOPNOTSUPP


def test_older_shared_engine_cannot_silently_drop_mount_configuration(monkeypatch):
    from vfsi_fsspec import VfsiFileSystem

    def old_constructor(self, host="127.0.0.1", **kwargs):
        pytest.fail("must reject an engine that cannot forward the pinned mount")

    monkeypatch.setattr(VfsiFileSystem, "__init__", old_constructor)
    with pytest.raises(ImportError, match="upgrade vfsi-fsspec"):
        Nfs4FileSystem(mount="/mnt/nfs", skip_instance_cache=True)


def test_mount_discovery_roundtrip_and_pooled_reconnect():
    mount = os.environ.get("VFSI_NFS_TEST_MOUNT")
    if not mount:
        pytest.skip("set VFSI_NFS_TEST_MOUNT to require live mount coverage")
    directory = Path(mount) / (".nfs4fs-mount-" + uuid.uuid4().hex)
    directory.mkdir()
    try:
        with fsspec.filesystem(
            "nfs4",
            mount=str(directory),
            connection_pool_size=2,
            skip_instance_cache=True,
        ) as fs:
            assert fs.auth == "auth_sys"
            assert not fs.read_only
            fs.pipe({"/file-1": b"hello", "/file-2": b"world"})
            assert fs.cat(["/file-1", "/file-2"]) == {
                "/file-1": b"hello",
                "/file-2": b"world",
            }
            fs._client.reconnect()
            assert fs.cat_file("/file-1") == b"hello"
            fs.rm(["/file-1", "/file-2"])
    finally:
        # Direct NFS mutations do not invalidate the mounted client's caches.
        # Only remove the now-empty root through the kernel during cleanup.
        directory.rmdir()


def test_read_only_mount_never_bypasses_the_kernel_restriction():
    mounted = os.environ.get("VFSI_NFS_TEST_MOUNT_RO")
    if not mounted:
        pytest.skip("set VFSI_NFS_TEST_MOUNT_RO to require read-only mount coverage")
    with fsspec.filesystem("nfs4", mount=mounted, skip_instance_cache=True) as fs:
        assert fs.read_only
        assert fs.info("/")["type"] == "directory"
        fs._client.reconnect()
        assert fs.info("/")["type"] == "directory"
        for mode in ["wb", "ab", "xb", "r+b"]:
            with pytest.raises(OSError) as error:
                fs.open("/.mount-forbidden", mode)
            assert error.value.errno == errno.EROFS
        with pytest.raises(OSError) as error:
            fs.pipe_file("/.mount-forbidden", b"must not be written")
        assert error.value.errno == errno.EROFS
        with pytest.raises(OSError) as error:
            fs.mkdir("/.mount-forbidden")
        assert error.value.errno == errno.EROFS


def test_replaced_mount_directory_is_rejected_before_reconnecting():
    mount = os.environ.get("VFSI_NFS_TEST_MOUNT")
    if not mount:
        pytest.skip("set VFSI_NFS_TEST_MOUNT to require live mount coverage")
    original = Path(mount) / (".nfs4fs-identity-" + uuid.uuid4().hex)
    saved = original.with_name(original.name + "-saved")
    original.mkdir()
    try:
        config = _native.discover_mount(str(original))
        original.rename(saved)
        original.mkdir()
        with pytest.raises(OSError, match="identity") as error:
            _native.NfsClient(
                config.host,
                minor_version=config.minor_version,
                auth="auth_sys",
                mount_config=config,
            )
        assert error.value.errno == errno.ESTALE
    finally:
        original.rmdir()
        if saved.exists():
            saved.rmdir()
