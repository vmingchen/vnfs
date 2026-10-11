"""Regressions from successive production readiness reviews."""

import pytest


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


@pytest.mark.parametrize("name", ["a" * 255, "é" * 127], ids=["ascii", "utf8"])
@pytest.mark.parametrize("mode", ["wb", "ab", "xb"])
def test_transaction_accepts_long_destination_names(fs, name, mode):
    path = "/" + name
    # Establish that the backend accepts the destination name itself.
    fs.pipe_file(path, b"original")
    if mode == "xb":
        fs.rm_file(path)
    with fs.transaction:
        with fs.open(path, mode) as file:
            file.write(b"new")
    assert fs.cat_file(path) == (b"originalnew" if mode == "ab" else b"new")
    assert fs.ls("/", detail=False) == ["nfs4://" + path]
    assert not fs._client._fds
