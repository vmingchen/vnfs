"""Run fsspec's upstream abstract contract against live vsmbfs."""

import posixpath

import pytest
from fsspec.tests.abstract import AbstractFixtures
from fsspec.tests.abstract.copy import AbstractCopyTests
from fsspec.tests.abstract.get import AbstractGetTests
from fsspec.tests.abstract.open import AbstractOpenTests
from fsspec.tests.abstract.pipe import AbstractPipeTests
from fsspec.tests.abstract.put import AbstractPutTests


class VsmbAbstractFixtures(AbstractFixtures):
    @pytest.fixture
    def fs(self, smb_fs):
        yield smb_fs

    @pytest.fixture
    def fs_join(self):
        return posixpath.join

    @pytest.fixture
    def fs_path(self):
        return "/"


class TestAbstractOpen(AbstractOpenTests, VsmbAbstractFixtures):
    pass


class TestAbstractPipe(AbstractPipeTests, VsmbAbstractFixtures):
    pass


class TestAbstractCopy(AbstractCopyTests, VsmbAbstractFixtures):
    pass


class TestAbstractGet(AbstractGetTests, VsmbAbstractFixtures):
    pass


class TestAbstractPut(AbstractPutTests, VsmbAbstractFixtures):
    pass
