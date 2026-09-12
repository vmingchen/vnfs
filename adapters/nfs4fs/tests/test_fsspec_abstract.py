"""Run fsspec's upstream abstract test suite against Nfs4FileSystem.

These are the same generic contract tests fsspec uses for its own
implementations (open/pipe/copy/get/put), run against both the local-directory
(dummy) backend and, when configured, live NFS.
"""

import os
import posixpath
import tempfile
import uuid

import fsspec
import pytest
from fsspec.tests.abstract import AbstractFixtures
from fsspec.tests.abstract.copy import AbstractCopyTests
from fsspec.tests.abstract.get import AbstractGetTests
from fsspec.tests.abstract.open import AbstractOpenTests
from fsspec.tests.abstract.pipe import AbstractPipeTests
from fsspec.tests.abstract.put import AbstractPutTests

from ._servers import nfs_config, nfs_reachable


class Nfs4AbstractFixtures(AbstractFixtures):
    @pytest.fixture(params=["dummy", "nfs"])
    def fs(self, request):
        if request.param == "dummy":
            with tempfile.TemporaryDirectory(prefix="nfs4fs_abstract_") as root:
                fs = fsspec.filesystem(
                    "nfs4", backend="dummy", dummy_root=root, auto_mkdir=True
                )
                yield fs
                fs.close()
            return
        host, minor_version = nfs_config()
        if not nfs_reachable(host, minor_version):
            if os.environ.get("VFSI_NFS_REQUIRED") == "1":
                pytest.fail(f"required NFS server {host!r} is not reachable")
            pytest.skip(f"NFS server {host!r} is not reachable")
        nfs_root = f"git/nfs4fs_abstract_{os.getpid()}_{uuid.uuid4().hex[:8]}"
        fs = fsspec.filesystem(
            "nfs4",
            host=host,
            root=nfs_root,
            auto_mkdir=True,
            minor_version=minor_version,
        )
        fs.mkdir("nfs4:///", create_parents=True)
        yield fs
        try:
            fs.rm("nfs4:///", recursive=True)
        except Exception:
            pass
        fs.close()

    @pytest.fixture
    def fs_join(self):
        return posixpath.join

    @pytest.fixture
    def fs_path(self):
        return "/"


class TestAbstractOpen(AbstractOpenTests, Nfs4AbstractFixtures):
    pass


class TestAbstractPipe(AbstractPipeTests, Nfs4AbstractFixtures):
    pass


class TestAbstractCopy(AbstractCopyTests, Nfs4AbstractFixtures):
    pass


class TestAbstractGet(AbstractGetTests, Nfs4AbstractFixtures):
    pass


class TestAbstractPut(AbstractPutTests, Nfs4AbstractFixtures):
    pass
