"""PRR round 9: bottom-up walk honors root error policy."""

import errno

import pytest


@pytest.mark.parametrize("on_error", ["omit", "callback"])
def test_bottomup_walk_handles_inaccessible_root(dummy_fs, monkeypatch, on_error):
    fs = dummy_fs
    native_path = fs._native_path("/blocked")

    def denied_walk(*args, **kwargs):
        raise PermissionError(errno.EACCES, "permission denied", "/blocked")

    original_stat_many = fs._client.stat_many

    def denied_stat(paths):
        if paths == [native_path]:
            return [None], {0: errno.EACCES}
        return original_stat_many(paths)

    monkeypatch.setattr(fs._client, "walk", denied_walk)
    monkeypatch.setattr(fs._client, "stat_many", denied_stat)
    handled = []
    policy = handled.append if on_error == "callback" else on_error

    assert list(fs.walk("/blocked", topdown=False, on_error=policy)) == []
    if on_error == "callback":
        assert len(handled) == 1
        assert handled[0].errno == errno.EACCES
        assert handled[0].filename == "/blocked"
    else:
        assert handled == []
