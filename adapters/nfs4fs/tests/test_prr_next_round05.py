"""PRR round 5: whole-file reads honor the active allocation budget."""

import errno
import uuid

import pytest
from vfsi_fsspec import VfsiFile
from vfsi_fsspec._fs import _RawVfsiFile


@pytest.fixture(params=["dummy_fs", "nfs_fs"])
def fs(request):
    return request.getfixturevalue(request.param)


def test_cat_file_honors_reduced_runtime_read_limit(fs):
    path = f"/prr-read-budget-{uuid.uuid4().hex}"
    fs.pipe_file(path, b"12345678")
    fs.read_all_max_total_bytes = 4

    with pytest.raises(OSError) as error:
        fs.cat_file(path)

    assert error.value.errno == errno.EFBIG
    assert fs.info(path)["size"] == 8


@pytest.mark.parametrize("reader", ["raw", "buffered"])
def test_unopened_whole_file_reader_honors_reduced_runtime_limit(fs, reader):
    path = f"/prr-read-budget-unopened-{uuid.uuid4().hex}"
    fs.pipe_file(path, b"12345678")
    fs.read_all_max_total_bytes = 4
    file = (
        _RawVfsiFile(fs, path, "rb")
        if reader == "raw"
        else VfsiFile(fs, path, "rb", cache_type="readahead")
    )

    try:
        with pytest.raises(OSError) as error:
            file.read()
        assert error.value.errno == errno.EFBIG
    finally:
        file.close()


def test_small_file_get_applies_active_batch_budget_after_source_growth(
    fs, tmp_path, monkeypatch
):
    remote = f"/prr-get-budget-{uuid.uuid4().hex}"
    destination = tmp_path / "download"
    fs.pipe_file(remote, b"aa")
    fs.read_all_max_total_bytes = 4
    native = fs._native_path(remote)
    original_stat_many = fs._client.stat_many
    grew = []

    def stat_then_grow(paths):
        result = original_stat_many(paths)
        if native in paths and not grew:
            grew.append(True)
            fs.pipe_file(remote, b"12345678")
        return result

    monkeypatch.setattr(fs._client, "stat_many", stat_then_grow)
    with pytest.raises(OSError) as error:
        fs.get([remote], [str(destination)])

    assert error.value.errno == errno.EFBIG
    assert grew
    assert not destination.exists()
    assert fs.info(remote)["size"] == 8
