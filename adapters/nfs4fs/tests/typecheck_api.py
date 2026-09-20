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
        use_listings_cache=True,
        listings_expiry_time=1.0,
        max_paths=16,
        read_all_max_total_bytes=4096,
        directory_max_entries=100,
        directory_max_path_bytes=4096,
        walk_max_depth=8,
        connection_pool_size=2,
    )
    fs.pipe_file("/file", b"data")
    info: dict[str, object] = fs.info("/file")
    assert info["size"] == 4
    data: bytes = fs.cat_file("/file", start=0, end=4)
    assert data == b"data"
    handle: Nfs4File = fs.open("/file", "rb")
    assert handle.read() == data
    handle.close()
    fs.ls("/", refresh=True)
    fs.invalidate_cache("/")
    fs.close()
    secure: Nfs4FileSystem = Nfs4FileSystem(
        host="nfs.example",
        authentication="krb5i",
        service_principal="nfs@nfs.example",
        require_secure_authentication=True,
    )
    secure.close()
