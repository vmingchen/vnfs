"""PRR round 16: public bounded range and allocation contracts."""

import errno
import pytest


@pytest.mark.parametrize("operation", ["cat_file", "cat_ranges"])
@pytest.mark.parametrize("end, expected", [(4, b"bcde"), (20, b"bcdef"), (0, b"b"[:0])])
def test_public_ranges_stop_at_requested_end_or_eof(dummy_fs, operation, end, expected):
    dummy_fs.pipe_file("/a", b"abcdef")
    stop = end + 1
    if operation == "cat_file":
        result = dummy_fs.cat_file("/a", start=1, end=stop)
    else:
        result = dummy_fs.cat_ranges(["/a"], [1], [stop])[0]
    assert result == expected


@pytest.mark.parametrize("operation", ["cat_file", "cat_ranges"])
def test_range_budget_rejection_happens_before_native_reads(
    dummy_fs, monkeypatch, operation
):
    dummy_fs.pipe_file("/a", b"abcdef")
    dummy_fs.read_all_max_total_bytes = 3
    calls = []
    read_many = dummy_fs._client.read_many

    def record(*args):
        calls.append(args)
        return read_many(*args)

    monkeypatch.setattr(dummy_fs._client, "read_many", record)
    with pytest.raises(OSError) as error:
        if operation == "cat_file":
            dummy_fs.cat_file("/a", start=0, end=6)
        else:
            dummy_fs.cat_ranges(["/a"], [0], [6])
    assert error.value.errno == errno.EFBIG
    assert not calls
