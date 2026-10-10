"""Production failure paths exercised on both local and live NFS backends."""

import os
import select
import socket
import socketserver
import threading
from contextlib import contextmanager
from urllib.parse import urlsplit

import pytest
from nfs4fs import Nfs4FileSystem


@pytest.fixture(params=["dummy", "nfs"])
def fs(request, tmp_path):
    if request.param == "nfs":
        yield request.getfixturevalue("nfs_fs")
    else:
        with Nfs4FileSystem(
            backend="dummy",
            dummy_root=str(tmp_path),
            root="prefix",
            skip_instance_cache=True,
        ) as filesystem:
            filesystem.makedirs("/", exist_ok=True)
            yield filesystem


@pytest.mark.parametrize("mode", ["wb+", "rb+", "ab+", "xb+"])
@pytest.mark.parametrize("auto_reconnect", [False, True])
def test_update_read_failure_never_reopens_or_replays_creation(
    fs, monkeypatch, mode, auto_reconnect
):
    fs.auto_reconnect = auto_reconnect
    if mode == "rb+":
        fs.pipe_file("/update", b"original")
    with fs.open("/update", mode) as file:
        if mode != "rb+":
            file.write(b"original")
        file.seek(0)
        fd = file._fd

        def lost_reply(*args):
            raise ConnectionError("injected lost read reply")

        with monkeypatch.context() as patch:
            patch.setattr(fs._client, "pread", lost_reply)
            with pytest.raises(ConnectionError, match="lost read reply"):
                file.read(3)
            assert file._fd == fd, "keep the original descriptor for cleanup"
            with pytest.raises(ConnectionError, match="unusable"):
                file.write(b"replacement")
        assert fs.cat_file("/update") == b"original"
    assert not fs._client.descriptor_valid(fd)


@pytest.mark.parametrize("manual", [False, True])
@pytest.mark.parametrize("competitor", ["file", "dangling_symlink"])
def test_exclusive_transaction_publication_cannot_replace_a_concurrent_creator(
    fs, monkeypatch, manual, competitor
):
    def arrange(file):
        file.write(b"transaction")
        prepare = file.prepare

        def prepare_then_competing_create():
            prepare()
            if competitor == "file":
                fs.pipe_file("/exclusive", b"competitor", mode="create")
            else:
                fs.symlink("missing", "/exclusive")

        monkeypatch.setattr(file, "prepare", prepare_then_competing_create)

    if manual:
        file = fs.open("/exclusive", "xb", autocommit=False)
        try:
            arrange(file)
            with pytest.raises(FileExistsError):
                file.commit()
        finally:
            file.discard()
    else:
        with pytest.raises(FileExistsError):
            with fs.transaction:
                with fs.open("/exclusive", "xb") as file:
                    arrange(file)
    if competitor == "file":
        assert fs.cat_file("/exclusive") == b"competitor"
    else:
        assert fs.readlink("/exclusive") == "missing"
    assert not any(".nfs4fs-txn-" in path for path in fs.ls("/", detail=False))


def test_mixed_transactions_keep_rename_batches_and_exclusive_publication(
    fs, monkeypatch
):
    renames = []
    links = []
    rename = fs._client.rename_many
    link = fs._client.hardlink

    def record_rename(pairs):
        renames.append([destination for _, destination in pairs])
        return rename(pairs)

    def record_link(source, destination):
        links.append(destination)
        return link(source, destination)

    monkeypatch.setattr(fs._client, "rename_many", record_rename)
    monkeypatch.setattr(fs._client, "hardlink", record_link)
    with fs.transaction:
        for path, mode in [("/a", "wb"), ("/b", "wb"), ("/c", "xb"), ("/d", "wb")]:
            with fs.open(path, mode) as file:
                file.write(path.encode())
    assert renames == [
        [fs._native_path("/a"), fs._native_path("/b")],
        [fs._native_path("/d")],
    ]
    assert links == [fs._native_path("/c")]
    for path in ["/a", "/b", "/c", "/d"]:
        assert fs.cat_file(path) == path.encode()
    assert not any(".nfs4fs-txn-" in path for path in fs.ls("/", detail=False))


def test_transaction_exclusive_collision_stops_later_publication(fs, monkeypatch):
    with pytest.raises(FileExistsError):
        with fs.transaction:
            for path, mode in [
                ("/before", "wb"),
                ("/collision", "xb"),
                ("/after", "wb"),
            ]:
                with fs.open(path, mode) as file:
                    file.write(b"data")
                if mode == "xb":
                    prepare = file.prepare

                    def race():
                        prepare()
                        fs.pipe_file("/collision", b"competitor", mode="create")

                    monkeypatch.setattr(file, "prepare", race)
    assert fs.cat_file("/before") == b"data"
    assert fs.cat_file("/collision") == b"competitor"
    assert not fs.exists("/after")
    assert not any(".nfs4fs-txn-" in path for path in fs.ls("/", detail=False))


def test_manual_exclusive_commit_is_idempotent_and_removes_staging(fs):
    file = fs.open("/manual", "xb", autocommit=False)
    file.write(b"data")
    file.close()
    file.commit()
    file.commit()
    assert fs.cat_file("/manual") == b"data"
    assert not any(".nfs4fs-txn-" in path for path in fs.ls("/", detail=False))


@pytest.mark.parametrize("retry", ["commit", "discard"])
@pytest.mark.parametrize("failure", [OSError, ConnectionError])
def test_exclusive_commit_retries_cleanup_without_republishing(
    fs, monkeypatch, retry, failure
):
    file = fs.open("/cleanup", "xb", autocommit=False)
    file.write(b"published")
    file.close()
    remove = fs.rm
    attempts = []

    def fail_unlink(path, **kwargs):
        assert path == file.temp_path
        attempts.append(path)
        raise failure("staging unlink failed")

    try:
        with monkeypatch.context() as patch:
            patch.setattr(fs, "rm", fail_unlink)
            with pytest.raises(failure, match="staging unlink failed"):
                file.commit()
        assert attempts == [file.temp_path]
        assert file._spool.closed
        assert fs.exists(file.temp_path)
        assert fs.cat_file("/cleanup") == b"published"

        # A retry must not republish even if another writer replaced the target.
        fs.rm("/cleanup")
        fs.pipe_file("/cleanup", b"replacement")

        def forbid_link(*args):
            pytest.fail("cleanup must not retry publication")

        with monkeypatch.context() as patch:
            patch.setattr(fs._client, "hardlink", forbid_link)
            getattr(file, retry)()
            file.commit()
        assert fs.cat_file("/cleanup") == b"replacement"
        assert not fs.exists(file.temp_path)
    finally:
        monkeypatch.setattr(fs, "rm", remove)
        file.discard()


@pytest.mark.parametrize("ambiguous", [False, True])
def test_exclusive_publication_failure_never_falls_back_or_replays(
    fs, monkeypatch, ambiguous
):
    link = fs._client.hardlink
    calls = []

    def fail(source, destination):
        calls.append((source, destination))
        if ambiguous:
            link(source, destination)
            raise ConnectionError("lost link reply")
        raise NotImplementedError("hard links unavailable")

    def forbid_rename(*args):
        pytest.fail("exclusive creation must not fall back to overwriting rename")

    monkeypatch.setattr(fs._client, "hardlink", fail)
    monkeypatch.setattr(fs._client, "rename_many", forbid_rename)
    with pytest.raises(ConnectionError if ambiguous else NotImplementedError):
        with fs.transaction:
            with fs.open("/exclusive", "xb") as file:
                file.write(b"complete")
    assert len(calls) == 1
    if ambiguous:
        assert fs.cat_file("/exclusive") == b"complete"
    else:
        assert not fs.exists("/exclusive")
    assert not any(".nfs4fs-txn-" in path for path in fs.ls("/", detail=False))


@pytest.mark.parametrize("target", ["/source/file", "file", os.fsdecode(b"file-\xff")])
def test_recursive_copy_preserves_stored_symlink_targets(fs, target):
    fs.mkdir("/source")
    filename = target.rsplit("/", 1)[-1]
    fs.pipe_file("/source/" + filename, b"payload")
    fs.symlink(target, "/source/link")
    stored = fs.readlink("/source/link")
    if not target.startswith("/"):
        assert os.fsencode(stored) == os.fsencode(target)
    assert fs.cat_file("/source/link") == b"payload"
    fs.copy("/source", "/copy", recursive=True, symlinks=True)
    assert os.fsencode(fs.readlink("/copy/link")) == os.fsencode(stored)
    assert fs.cat_file("/copy/link") == b"payload"


@pytest.mark.parametrize("parents", [False, True])
def test_mkdir_honors_zero_permissions(fs, parents):
    fs.mkdir("/private", create_parents=parents, mode=0)
    try:
        assert fs.info("/private")["mode"] & 0o7777 == 0
    finally:
        fs._client.chmod(fs._native_path("/private"), 0o700)


@pytest.mark.parametrize("mode", [0, 0o600])
def test_mkdir_restrictive_leaf_keeps_missing_ancestors_traversable(fs, mode):
    fs.mkdir("/existing", mode=0o700)
    paths = [
        "/existing/parent",
        "/existing/parent/child",
        "/existing/parent/child/leaf",
    ]
    try:
        fs.mkdir(paths[-1], mode=mode)
        assert fs.info(paths[-1])["mode"] & 0o7777 == mode
        assert fs.info("/existing")["mode"] & 0o7777 == 0o700
        for parent in paths[:-1]:
            assert fs.info(parent)["mode"] & 0o700 == 0o700
    finally:
        for path in paths:
            if fs.exists(path):
                fs._client.chmod(fs._native_path(path), 0o700)


@contextmanager
def dropping_proxy(host):
    """Drop one established client RPC without disrupting the shared server."""
    address = urlsplit("//" + host)
    cut = threading.Event()
    stopped = threading.Event()
    accepted = []

    class Relay(socketserver.BaseRequestHandler):
        def handle(self):
            accepted.append(1)
            try:
                with socket.create_connection(
                    (address.hostname, address.port or 2049)
                ) as upstream:
                    while not stopped.is_set():
                        ready, _, _ = select.select(
                            [self.request, upstream], [], [], 0.05
                        )
                        for source in ready:
                            data = source.recv(65536)
                            if not data:
                                return
                            if source is self.request and cut.is_set():
                                cut.clear()
                                return
                            destination = (
                                upstream if source is self.request else self.request
                            )
                            destination.sendall(data)
            except OSError:
                return

    class Server(socketserver.ThreadingTCPServer):
        daemon_threads = True

    with Server(("127.0.0.1", 0), Relay) as server:
        thread = threading.Thread(
            target=server.serve_forever, kwargs={"poll_interval": 0.05}
        )
        thread.start()
        try:
            yield f"127.0.0.1:{server.server_address[1]}", cut, accepted
        finally:
            stopped.set()
            server.shutdown()
            thread.join(timeout=5)


@pytest.mark.parametrize("reconnect", [False, True])
@pytest.mark.parametrize(
    "operation", ["stat", "range", "whole", "descriptor", "update"]
)
def test_recovery_policy_controls_native_rpc_reconnects(nfs_fs, reconnect, operation):
    nfs_fs.pipe_file("/recovery", b"data")
    with dropping_proxy(nfs_fs.host) as (host, cut, accepted):
        with Nfs4FileSystem(
            host=host,
            root=nfs_fs._root,
            auth="auth_sys",
            auto_reconnect=reconnect,
            request_timeout=0.25,
            connect_timeout=2,
            skip_instance_cache=True,
        ) as filesystem:
            reader = None
            if operation in ("descriptor", "update"):
                reader = filesystem.open(
                    "/recovery",
                    "wb+" if operation == "update" else "rb",
                    cache_type="none",
                )
                if operation == "update":
                    reader.write(b"data")
                    reader.seek(0)
            try:
                calls = {
                    "stat": lambda: filesystem.info("/recovery")["size"],
                    "range": lambda: filesystem.cat_file("/recovery", start=0, end=4),
                    "whole": lambda: filesystem.cat_file("/recovery"),
                    "descriptor": lambda: reader.read(4),
                    "update": lambda: reader.read(4),
                }
                assert len(accepted) == 1
                cut.set()
                if reconnect:
                    assert calls[operation]() == (4 if operation == "stat" else b"data")
                    assert len(accepted) == 2
                else:
                    with pytest.raises(ConnectionError):
                        calls[operation]()
                    assert len(accepted) == 1
                assert nfs_fs.cat_file("/recovery") == b"data"
            finally:
                # The deliberately dead session cannot acknowledge CLOSE.
                # Filesystem shutdown owns cleanup after this test failure.
                if reader is not None and reconnect:
                    reader.close()
