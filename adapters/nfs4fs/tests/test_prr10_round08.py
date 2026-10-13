"""PRR round 8: export paths have one canonical leading separator."""

import posixpath

import fsspec
import pytest


@pytest.mark.parametrize("prefix", ["", "namespace"])
@pytest.mark.parametrize(
    "path", ["/tree", "//tree", "nfs4:////tree", "nfs4://127.0.0.1//tree"]
)
def test_repeated_leading_slashes_keep_metadata_and_walk_consistent(
    tmp_path, prefix, path
):
    with fsspec.filesystem(
        "nfs4",
        backend="dummy",
        dummy_root=str(tmp_path),
        root=prefix,
        use_listings_cache=True,
        skip_instance_cache=True,
    ) as fs:
        fs.mkdir("/tree/deep", create_parents=True)
        fs.pipe({"/tree/file": b"parent", "/tree/deep/child": b"child"})
        fs.ls(path)
        fs.pipe_file("/tree/added", b"added")
        assert "added" in [
            posixpath.basename(name) for name in fs.ls(path, detail=False)
        ]
        assert list(fs.walk(path, topdown=False)) == [
            ("/tree/deep", [], ["child"]),
            ("/tree", ["deep"], ["added", "file"]),
        ]
        assert list(fs.walk(path)) == [
            ("/tree", ["deep"], ["added", "file"]),
            ("/tree/deep", [], ["child"]),
        ]
        assert fs.info(path)["name"] == "nfs4:///tree"
        assert fs.find(path) == ["/tree/added", "/tree/deep/child", "/tree/file"]
        assert fs.cat_file(path + "/file") == b"parent"
