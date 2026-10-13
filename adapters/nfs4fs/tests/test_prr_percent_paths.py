"""Metadata and traversal paths round-trip literal percent escapes."""

import pytest


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


@pytest.mark.parametrize("name", ["report%20.txt", "part%2Fname", "percent%25.bin"])
def test_percent_paths_roundtrip_through_metadata_and_traversal(fs, name):
    directory = "/dir%2Fdata"
    path = f"{directory}/{name}"
    fs.mkdir(directory)
    fs.pipe_file(path, b"literal percent data")

    directory_url = fs.info(directory)["name"]
    file_url = fs.info(path)["name"]
    assert fs._strip_protocol(directory_url) == directory
    assert fs._strip_protocol(file_url) == path
    assert fs.cat_file(file_url) == b"literal percent data"
    assert fs.ls(directory_url, detail=False) == [file_url]
    assert fs.find(directory_url) == [path]
    assert fs.glob(directory_url + "/*") == [path]
    assert fs.expand_path(directory_url, recursive=True) == sorted([directory, path])
    with fs.open(file_url, "rb") as file:
        assert file.read() == b"literal percent data"
    assert list(fs.walk("/")) == [
        ("/", ["dir%2Fdata"], []),
        (directory, [], [name]),
    ]
    assert list(fs.walk("/", topdown=False)) == [
        (directory, [], [name]),
        ("/", ["dir%2Fdata"], []),
    ]
    assert fs.cat_file("nfs4://" + path.replace("%", "%25")) == b"literal percent data"
