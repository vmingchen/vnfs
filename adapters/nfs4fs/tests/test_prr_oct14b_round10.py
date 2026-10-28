"""Native copy recovery reports uncertainty without replaying mutations."""

import errno

import pytest


def sources(fs):
    for path in ["/a", "/b", "/c"]:
        fs.pipe_file(path, b"abcdef")


def test_native_copy_verifies_completed_prefix_without_replay(dummy_fs):
    sources(dummy_fs)
    copied, errors = dummy_fs._client.copy_many(
        [("/a", "/x"), ("/b", "/missing/y"), ("/c", "/z")]
    )
    assert copied == [6, None, 6]
    assert set(errors) == {1}
    assert isinstance(errors[1], FileNotFoundError)
    assert errors[1].errno == errno.ENOENT
    assert dummy_fs.cat_file("/x") == dummy_fs.cat_file("/z") == b"abcdef"


def test_first_copy_failure_has_no_uncertain_prefix(dummy_fs):
    sources(dummy_fs)
    copied, errors = dummy_fs._client.copy_many([("/a", "/missing/y"), ("/b", "/z")])
    assert copied == [None, 6]
    assert set(errors) == {0}
    assert isinstance(errors[0], FileNotFoundError)
    assert errors[0].errno == errno.ENOENT


@pytest.mark.parametrize("on_error", ["ignore", "raise"])
def test_public_copy_preserves_successful_suffix_on_native_error(dummy_fs, on_error):
    sources(dummy_fs)
    dummy_fs.auto_mkdir = False
    if on_error == "raise":
        with pytest.raises(OSError) as error:
            dummy_fs.copy(
                ["/a", "/b", "/c"], ["/x", "/missing/y", "/z"], on_error=on_error
            )
        assert error.value.errno == errno.ENOENT
    else:
        dummy_fs.copy(["/a", "/b", "/c"], ["/x", "/missing/y", "/z"], on_error=on_error)
    assert dummy_fs.cat_file("/x") == dummy_fs.cat_file("/z") == b"abcdef"
