"""PRR round 7: same-path moves still validate the source namespace."""

import pytest


@pytest.mark.parametrize(
    "source", ["/missing", "nfs4:///missing", "/parent/../missing"]
)
def test_same_path_move_rejects_missing_source(dummy_fs, source):
    with pytest.raises(FileNotFoundError):
        dummy_fs.mv(source, "/missing")
    assert dummy_fs.ls("/", detail=False) == []


@pytest.mark.parametrize("kind", ["file", "directory"])
def test_same_path_move_preserves_existing_object(dummy_fs, kind):
    if kind == "directory":
        dummy_fs.mkdir("/existing")
        dummy_fs.pipe_file("/existing/child", b"contents")
        content_path = "/existing/child"
    else:
        dummy_fs.pipe_file("/existing", b"contents")
        content_path = "/existing"
    before = dummy_fs.info("/existing")["fileid"]
    dummy_fs.mv("nfs4:///existing", "/existing")
    assert dummy_fs.info("/existing")["fileid"] == before
    assert dummy_fs.cat_file(content_path) == b"contents"
