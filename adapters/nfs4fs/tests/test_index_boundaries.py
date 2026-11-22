"""Attribute errors across parent preflights and grouped recovery."""

import errno

import pytest
from fsspec.core import OpenFile


@pytest.mark.parametrize(
    "operation",
    [
        "pipe",
        "exclusive",
        "oversized",
        "put_streamed",
        "put_dirs",
        "open_many",
        "copy_link",
    ],
)
def test_parent_preflight_keeps_public_input_position(dummy_fs, tmp_path, operation):
    fs = dummy_fs
    fs.pipe({"/occupied": b"file", "/ok": b"existing", "/target": b"target"})
    fs.auto_mkdir = True
    fs.batch_size = 1
    bad = "/occupied/child/file"
    if operation in ("oversized", "put_streamed"):
        fs.max_batch_bytes = 2
    with pytest.raises(NotADirectoryError) as raised:
        if operation in ("pipe", "exclusive", "oversized"):
            fs.pipe(
                {"/out": b"ok", bad: b"bad"},
                mode="create" if operation == "exclusive" else "overwrite",
            )
        elif operation in ("put_streamed", "put_dirs"):
            local = tmp_path / "local"
            local.write_bytes(b"ok")
            other = tmp_path / "other"
            if operation == "put_dirs":
                other.mkdir()
            else:
                other.write_bytes(b"bad")
            fs.put([str(local), str(other)], ["/out", bad])
        elif operation == "open_many":
            fs.open_many([OpenFile(fs, "/ok", mode="rb"), OpenFile(fs, bad, mode="wb")])
        else:
            fs.symlink("/target", "/link")
            fs.copy(["/ok", "/link"], ["/out", bad], symlinks=True)
    assert raised.value.index == 1
    assert not fs._client._fds
    assert not fs._client._deferred_close


@pytest.mark.parametrize("recover_individually", [False, True])
def test_copy_retry_parent_transport_error_keeps_public_position(
    dummy_fs, monkeypatch, recover_individually
):
    fs = dummy_fs
    fs.pipe({"/first": b"first", "/later": b"later", "/last": b"last"})
    fs.mkdir("/directory")
    fs.auto_mkdir = True
    original_copy = fs._client.copy_many
    original_mkdir = fs._client.mkdir_many
    failure = TimeoutError("lost parent creation reply")
    failure.transport_kind = "timeout"
    failure.index = 0
    copies = []
    mkdirs = []

    def copy(pairs):
        copies.append(list(pairs))
        return original_copy(pairs)

    def mkdir(paths, mode):
        mkdirs.append(list(paths))
        if recover_individually and len(mkdirs) == 1:
            error = PermissionError(errno.EACCES, "batch parent denied")
            error.index = 0
            raise error
        if recover_individually and paths == ["/a"]:
            return original_mkdir(paths, mode)
        raise failure

    monkeypatch.setattr(fs._client, "copy_many", copy)
    monkeypatch.setattr(fs._client, "mkdir_many", mkdir)
    sources = ["/directory", "/first", "/later"]
    destinations = ["/", "/out", "/a/new"]
    if recover_individually:
        sources.append("/last")
        destinations.append("/b/new")
    with pytest.raises(TimeoutError) as raised:
        fs.copy(sources, destinations, symlinks=True)
    assert raised.value is failure
    assert failure.index == (3 if recover_individually else 2)
    assert failure.transport_kind == "timeout"
    assert len(copies) == 1
    assert fs.cat_file("/out") == b"first"


@pytest.mark.parametrize("native_index", [0, 1, 2, 3, None, 99, True])
def test_link_identity_probe_rebases_flattened_positions(
    dummy_fs, monkeypatch, native_index
):
    fs = dummy_fs
    fs.pipe({"/first": b"first", "/target-a": b"a", "/target-b": b"b"})
    fs.symlink("/target-a", "/link-a")
    fs.symlink("/target-b", "/link-b")
    failure = TimeoutError("lost link identity reply")
    failure.transport_kind = "timeout"
    failure.index = native_index
    calls = []

    def stat(paths):
        calls.append(list(paths))
        raise failure

    monkeypatch.setattr(fs._client, "stat_many", stat)
    with pytest.raises(TimeoutError) as raised:
        fs.copy(
            ["/first", "/link-a", "/link-b"],
            ["/out", "/copy-a", "/copy-b"],
            symlinks=True,
        )
    assert raised.value is failure
    expected = (
        [1, 1, 2, 2][native_index]
        if type(native_index) is int and 0 <= native_index < 4
        else native_index
    )
    assert failure.index == expected
    assert calls == [["/link-a", "/copy-a", "/link-b", "/copy-b"]]
    assert fs.cat_file("/out") == b"first"


@pytest.mark.parametrize("on_error", ["return", "raise"])
def test_invalid_range_metadata_error_keeps_public_position(dummy_fs, on_error):
    dummy_fs.pipe_file("/ok", b"data")
    if on_error == "return":
        result = dummy_fs.cat_ranges(
            ["/ok", "/missing"], [0, 2], [1, 0], on_error=on_error
        )
        assert result[0] == b"d"
        failure = result[1]
        assert isinstance(failure, FileNotFoundError)
    else:
        with pytest.raises(FileNotFoundError) as raised:
            dummy_fs.cat_ranges(["/ok", "/missing"], [0, 2], [1, 0], on_error=on_error)
        failure = raised.value
    assert failure.index == 1
    assert failure.filename == "/missing"


@pytest.mark.parametrize("phase", ["open", "fstat"])
def test_group_restore_rebases_active_reader_positions(
    dummy_fs, tmp_path, monkeypatch, phase
):
    fs = dummy_fs
    fs.pipe({"/a": b"aaaa", "/b": b"bbbb", "/c": b"cccc"})
    files = fs.open_many([OpenFile(fs, path, mode="rb") for path in ["/a", "/b", "/c"]])
    original_open = fs._client.open_many
    opens = []

    def open_many(paths, modes):
        opens.append(list(paths))
        return original_open(paths, modes)

    try:
        files[0].close()
        fs._client.reconnect_descriptor(files[1]._raw._fd)
        monkeypatch.setattr(fs._client, "open_many", open_many)
        if phase == "open":
            (tmp_path / "root" / "c").unlink()
            expected = OSError
            failure = None
        else:
            failure = PermissionError(errno.EACCES, "restored handle metadata denied")
            failure.index = 0
            failure.status_domain = "nfs"
            failure.status_code = 13
            expected = PermissionError
            original_fstat = fs._client.fstat
            original_fstat_many = fs._client.fstat_many
            scalar_calls = []

            def fstat_many(fds):
                if len(fds) == 2:
                    raise failure
                return original_fstat_many(fds)

            def fstat(fd):
                scalar_calls.append(fd)
                if len(scalar_calls) == 1:
                    raise failure
                return original_fstat(fd)

            monkeypatch.setattr(fs._client, "fstat_many", fstat_many)
            monkeypatch.setattr(fs._client, "fstat", fstat)
        with pytest.raises(expected) as raised:
            files[2].read(1)
        assert raised.value.index == 2
        assert raised.value.filename == "/c"
        if failure is None:
            assert raised.value.errno == errno.ESTALE
        else:
            assert raised.value is failure
            assert (failure.status_domain, failure.status_code) == ("nfs", 13)
        assert opens == [["/c", "/b"]]
        assert files[1].read(1) == b"b"
    finally:
        fs.commit_many(files)


@pytest.mark.parametrize("phase", ["initial", "continuation"])
def test_legacy_group_read_rebases_selected_and_continuation_positions(
    dummy_fs, monkeypatch, phase
):
    fs = dummy_fs
    fs.pipe({"/a": b"aaaa", "/b": b"bbbb", "/c": b"cccc"})
    fs.auto_reconnect = False
    monkeypatch.setattr(fs._client, "_native_has_vector_outcomes", False)
    files = fs.open_many([OpenFile(fs, path, mode="rb") for path in ["/a", "/b", "/c"]])
    failure = TimeoutError("legacy read reply lost")
    failure.index = 0
    failure.transport_kind = "timeout"
    original = fs._client.pread_many
    calls = []

    def pread(fds, offsets, lengths):
        calls.append((list(offsets), list(lengths)))
        if phase == "continuation" and len(calls) == 1:
            data, errors = original(fds, offsets, lengths)
            data[0] = data[0][:1]
            return data, errors
        raise failure

    try:
        files[0].close()
        monkeypatch.setattr(fs._client, "pread_many", pread)
        with pytest.raises(TimeoutError) as raised:
            files[2].read(2)
        assert raised.value is failure
        assert failure.index == 2
        assert len(calls) == (2 if phase == "continuation" else 1)
    finally:
        fs.commit_many(files)
    assert not fs._client._fds


@pytest.mark.parametrize("phase", ["append_constructor", "exclusive_prepare"])
def test_deferred_scalar_probe_does_not_claim_first_grouped_input(
    dummy_fs, tmp_path, monkeypatch, phase
):
    import os

    fs = dummy_fs
    fs.mkdir("/blocked")
    fs.pipe({"/ok": b"old", "/blocked/file": b"retained"})
    original = fs._client.exists_many
    failures = []

    def exists(paths):
        try:
            return original(paths)
        except PermissionError as error:
            failures.append(
                (error, dict(error.__dict__), error.__cause__, error.filename)
            )
            raise

    monkeypatch.setattr(fs._client, "exists_many", exists)
    os.chmod(tmp_path / "root" / "blocked", 0)
    try:
        with pytest.raises(PermissionError) as raised:
            with fs.transaction:
                mode = "ab" if phase == "append_constructor" else "xb"
                first = "/ok" if mode == "ab" else "/ok-new"
                files = fs.open_many(
                    [
                        OpenFile(fs, first, mode=mode),
                        OpenFile(fs, "/blocked/file", mode=mode),
                    ]
                )
                for file in files:
                    file.write(b"new")
                    file.close()
        assert len(failures) == 1
        error, attributes, cause, filename = failures[0]
        assert raised.value is error
        assert error.errno == errno.EACCES
        assert error.filename == filename
        assert filename.endswith("/blocked/file")
        assert error.__cause__ is cause
        assert getattr(error, "index", None) is None
        assert {
            key: value for key, value in error.__dict__.items() if key != "index"
        } == {key: value for key, value in attributes.items() if key != "index"}
    finally:
        os.chmod(tmp_path / "root" / "blocked", 0o755)
    assert fs.cat_file("/ok") == b"old"
    assert fs.cat_file("/blocked/file") == b"retained"
    assert not fs.exists("/ok-new")
    assert not any(".nfs4fs-txn-" in path for path in fs.ls("/", detail=False))
    assert not fs._client._fds
    assert not fs._intrans
    assert fs._transaction is None


def test_deferred_parent_creation_does_not_claim_first_transaction_file(
    dummy_fs, tmp_path
):
    import os

    fs = dummy_fs
    fs.mkdir("/readonly")
    fs.auto_mkdir = True
    os.chmod(tmp_path / "root" / "readonly", 0o555)
    try:
        with pytest.raises(PermissionError) as raised:
            with fs.transaction:
                for path in ["/healthy", "/readonly/new/file"]:
                    with fs.open(path, "wb") as file:
                        file.write(b"data")
        assert raised.value.errno == errno.EACCES
        assert getattr(raised.value, "index", None) is None
        assert raised.value.filename.endswith("/readonly/new")
    finally:
        os.chmod(tmp_path / "root" / "readonly", 0o755)
    assert not fs.exists("/healthy")
    assert not fs.exists("/readonly/new")
    assert not any(".nfs4fs-txn-" in path for path in fs.ls("/", detail=False))
    assert not fs._client._fds
    assert not fs._intrans
    assert fs._transaction is None


@pytest.mark.parametrize("action", ["commit", "discard"])
def test_deferred_manual_finalization_keeps_scalar_error_context(
    dummy_fs, tmp_path, action
):
    import os

    fs = dummy_fs
    fs.mkdir("/readonly")
    fs.pipe_file("/readonly/file", b"retained")
    with fs.transaction:
        with fs.open("/healthy", "wb") as healthy:
            healthy.write(b"healthy")
        with fs.open("/readonly/file", "wb") as file:
            file.write(b"replacement")
        file.prepare()
        os.chmod(tmp_path / "root" / "readonly", 0o555)
        try:
            with pytest.raises(PermissionError) as raised:
                getattr(file, action)()
            assert raised.value.errno == errno.EACCES
            assert getattr(raised.value, "index", None) is None
        finally:
            os.chmod(tmp_path / "root" / "readonly", 0o755)
            file.discard()
    assert fs.cat_file("/healthy") == b"healthy"
    assert fs.cat_file("/readonly/file") == b"retained"
    assert not any(".nfs4fs-txn-" in path for path in fs.ls("/readonly", detail=False))
    assert not fs._client._fds
    assert not fs._intrans
    assert fs._transaction is None


def test_symlink_copy_creation_failure_has_original_index(dummy_fs):
    dummy_fs.pipe({"/file": b"file", "/exists": b"untouched"})
    dummy_fs.symlink("/file", "/link")
    with pytest.raises(FileExistsError) as caught:
        dummy_fs.copy(["/file", "/link"], ["/copy", "/exists"], symlinks=True)
    assert caught.value.index == 1
    assert dummy_fs.cat_file("/copy") == b"file"
    assert dummy_fs.cat_file("/exists") == b"untouched"
