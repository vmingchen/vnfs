"""Standalone contract tests for the backend-neutral vfsi-fsspec package."""

import errno
import inspect
from types import SimpleNamespace

import pytest
from vfsi_fsspec import VfsiFileSystem


class _MemoryClient:
    instances = []

    def __init__(self, *factory_args):
        self.factory_args = factory_args
        self.files = {"/alpha": b"alpha", "/beta": b"beta"}
        self.calls = []
        self.was_shutdown = False
        type(self).instances.append(self)

    def shutdown(self):
        self.was_shutdown = True

    def stat_many(self, paths):
        self.calls.append(("stat_many", list(paths)))
        stats = []
        errors = {}
        for index, path in enumerate(paths):
            data = self.files.get(path)
            if data is None:
                stats.append(None)
                errors[index] = errno.ENOENT
            else:
                stats.append({"name": path, "type": "file", "size": len(data)})
        return stats, errors

    def exists_many(self, paths):
        self.calls.append(("exists_many", list(paths)))
        return [path in self.files for path in paths]

    def read_all_many(self, paths, max_total_bytes=None):
        self.calls.append(("read_all_many", list(paths), max_total_bytes))
        values = []
        errors = {}
        for index, path in enumerate(paths):
            if path in self.files:
                values.append(self.files[path])
            else:
                values.append(None)
                errors[index] = errno.ENOENT
        return values, errors

    def read_many(self, paths, starts, ends):
        self.calls.append(("read_many", list(paths), list(starts), list(ends)))
        values = []
        errors = {}
        for index, (path, start, end) in enumerate(zip(paths, starts, ends)):
            if path in self.files:
                values.append(self.files[path][start:end])
            else:
                values.append(None)
                errors[index] = errno.ENOENT
        return values, errors


class _MemoryFileSystem(VfsiFileSystem):
    protocol = "memory-vfsi"
    _native_module = SimpleNamespace(NfsClient=_MemoryClient)
    _supported_backends = frozenset({"memory"})


def test_base_class_requires_a_protocol_adapter():
    with pytest.raises(TypeError, match="backend base class"):
        VfsiFileSystem(backend="dummy")


def test_adapter_lifecycle_and_factory_contract():
    fs = _MemoryFileSystem(backend="memory", host="server", root="tenant")
    client = _MemoryClient.instances[-1]

    assert client.factory_args[0:2] == ("server", "memory")
    assert fs._native_path("/alpha") == "/tenant/alpha"
    fs.close()
    assert client.was_shutdown
    assert fs.closed


def test_vector_reads_use_the_backend_batch_contract():
    fs = _MemoryFileSystem(backend="memory", root="")
    client = _MemoryClient.instances[-1]
    try:
        assert fs.cat(["/alpha", "/beta"]) == {
            "/alpha": b"alpha",
            "/beta": b"beta",
        }
        assert fs.cat_ranges(["/alpha", "/beta"], [1, 0], [4, 2]) == [b"lph", b"be"]
        assert fs.exists("/alpha")
        assert not fs.exists("/missing")
    finally:
        fs.close()

    assert (
        "read_all_many",
        ["/alpha", "/beta"],
        fs.read_all_max_total_bytes,
    ) in client.calls
    assert (
        "read_many",
        ["/alpha", "/beta"],
        [1, 0],
        [4, 2],
    ) in client.calls


def test_info_preserves_unavailable_posix_identity_fields():
    fs = _MemoryFileSystem(backend="memory", root="")
    try:
        info = fs.info("/alpha")
    finally:
        fs.close()

    assert "uid" in info and info["uid"] is None
    assert "gid" in info and info["gid"] is None


@pytest.mark.parametrize("reconnect", [False, True])
def test_legacy_nfs_constructor_compatibility(reconnect):
    class LegacyClient(_MemoryClient):
        instances = []
        # The previous PyO3 constructor has 18 positional parameters.
        __signature__ = inspect.Signature(
            [
                inspect.Parameter(f"p{i}", inspect.Parameter.POSITIONAL_ONLY)
                for i in range(18)
            ]
        )

        def __init__(self, *args):
            assert len(args) <= 18
            super().__init__(*args)

    class LegacyFs(VfsiFileSystem):
        _native_module = SimpleNamespace(NfsClient=LegacyClient)
        _supported_backends = frozenset({"nfs"})

    kwargs = dict(
        host="server",
        backend="nfs",
        auth="auth_sys",
        auto_reconnect=reconnect,
        skip_instance_cache=True,
    )
    if reconnect:
        with LegacyFs(**kwargs) as fs:
            assert fs.cat_file("/alpha") == b"alpha"
        assert len(LegacyClient.instances) == 1
        assert LegacyClient.instances[0].was_shutdown
    else:
        with pytest.raises(ImportError, match="upgrade nfs4fs alongside vfsi-fsspec"):
            LegacyFs(**kwargs)
        assert not LegacyClient.instances, "fail before opening a connection"
