"""PRR round 7: whole-file reads trust actual bounded read results."""

import pytest


def test_cat_accepts_file_that_shrinks_after_planning_stat(dummy_fs, monkeypatch):
    fs = dummy_fs
    fs.read_all_max_total_bytes = 4
    fs.pipe_file("/shrinking", b"12345")
    original_stat_many = fs._client.stat_many
    changed = False

    def shrink_after_stat(paths):
        nonlocal changed
        result = original_stat_many(paths)
        if not changed and paths == [fs._native_path("/shrinking")]:
            changed = True
            fs.pipe_file("/shrinking", b"a")
        return result

    monkeypatch.setattr(fs._client, "stat_many", shrink_after_stat)

    assert fs.cat(["/shrinking"]) == {"/shrinking": b"a"}


def test_cat_reports_directory_error_before_comparing_directory_size(dummy_fs):
    fs = dummy_fs
    fs.read_all_max_total_bytes = 1
    fs.makedirs("/directory")
    assert fs.info("/directory")["size"] > fs.read_all_max_total_bytes

    with pytest.raises(IsADirectoryError):
        fs.cat(["/directory"])
