"""Run fsspec's upstream abstract test suite against Nfs4FileSystem.

These are the same generic contract tests fsspec uses for its own
implementations (open/pipe/copy/get/put), run against both the local-directory
(dummy) backend and, when the local NFSv4.1 server is reachable, the NFS
backend.
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


def _nfs_reachable():
    from nfs4fs import _native

    try:
        _native.NfsClient("127.0.0.1", "nfs")
        return True
    except Exception:
        return False


class Nfs4AbstractFixtures(AbstractFixtures):
    @pytest.fixture(params=["dummy", "nfs"])
    def fs(self, request):
        if request.param == "dummy":
            root = tempfile.mkdtemp(prefix="nfs4fs_abstract_")
            yield fsspec.filesystem(
                "nfs4", backend="dummy", dummy_root=root, auto_mkdir=True
            )
            return
        if not _nfs_reachable():
            pytest.skip("local NFSv4.1 server (127.0.0.1) is not reachable")
        nfs_root = f"git/nfs4fs_abstract_{os.getpid()}_{uuid.uuid4().hex[:8]}"
        fs = fsspec.filesystem("nfs4", host="127.0.0.1", root=nfs_root, auto_mkdir=True)
        fs.mkdir("nfs4:///", create_parents=True)
        yield fs
        try:
            fs.rm("nfs4:///", recursive=True)
        except Exception:
            pass

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
