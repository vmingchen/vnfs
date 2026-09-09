"""Static API smoke test; this file is checked by mypy in CI."""

from pathlib import Path

from nfs4fs import Nfs4File, Nfs4FileSystem
from vfsi import VfsiFileSystem


def use_api(root: Path) -> None:
    fs: Nfs4FileSystem = VfsiFileSystem(
        backend="dummy",
        dummy_root=root,
        batch_size=32,
        max_batch_bytes=1024,
        transfer_chunk_size=512,
        connect_timeout=2.0,
        request_timeout=3.0,
    )
    fs.pipe_file("/file", b"data")
    info: dict[str, object] = fs.info("/file")
    assert info["size"] == 4
    data: bytes = fs.cat_file("/file", start=0, end=4)
    assert data == b"data"
    handle: Nfs4File = fs.open("/file", "rb")
    assert handle.read() == data
    handle.close()
    fs.close()
