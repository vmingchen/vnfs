"""PRR round 14: partial native copy errors remain visible to public callers."""

import errno

import pytest

from .test_callbacks import RecordingCallback


@pytest.mark.parametrize("written", [0, 2])
def test_scalar_short_copy_error_never_reports_completion(
    dummy_fs, monkeypatch, written
):
    dummy_fs.pipe_file("/src", b"abcdef")
    calls = []

    def partial_copy(pairs):
        calls.append(pairs)
        dummy_fs._client.write_many([pairs[0][1]], [b"abcdef"[:written]], truncate=True)
        return [None], {0: errno.EIO}

    monkeypatch.setattr(dummy_fs._client, "copy_many", partial_copy)
    callback = RecordingCallback()
    with pytest.raises(OSError) as error:
        dummy_fs.cp_file("/src", "/dst", callback=callback)
    assert error.value.errno == errno.EIO
    assert dummy_fs.cat_file("/dst") == b"abcdef"[:written]
    assert callback.value == 0
    assert len(calls) == 1


@pytest.mark.parametrize("written", [0, 2])
@pytest.mark.parametrize("failed_index", [0, 1])
@pytest.mark.parametrize("on_error", ["raise", "ignore"])
def test_vector_short_copy_errors_preserve_success_and_progress(
    dummy_fs, monkeypatch, written, failed_index, on_error
):
    dummy_fs.pipe({"/a": b"abcdef", "/b": b"abcdef"})
    calls = []

    def partial_copy(pairs):
        calls.append(pairs)
        for index, (_, destination) in enumerate(pairs):
            dummy_fs._client.write_many(
                [destination],
                [b"abcdef"[:written] if index == failed_index else b"abcdef"],
                truncate=True,
            )
        copied = [6, 6]
        copied[failed_index] = None
        return copied, {failed_index: errno.EIO}

    monkeypatch.setattr(dummy_fs._client, "copy_many", partial_copy)
    callback = RecordingCallback()
    if on_error == "raise":
        with pytest.raises(OSError) as error:
            dummy_fs.copy(
                ["/a", "/b"], ["/x", "/y"], on_error=on_error, callback=callback
            )
        assert error.value.errno == errno.EIO
    else:
        dummy_fs.copy(["/a", "/b"], ["/x", "/y"], on_error=on_error, callback=callback)
    assert len(calls) == 1
    assert callback.value == 2
    assert len(callback.children) == 1
    assert callback.children[0].value == 6
    assert dummy_fs.cat_file(["/x", "/y"][failed_index]) == b"abcdef"[:written]
    assert dummy_fs.cat_file(["/x", "/y"][1 - failed_index]) == b"abcdef"
