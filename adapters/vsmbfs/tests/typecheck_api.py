"""Static public-API smoke test; mypy checks this file in CI."""

from vsmbfs import VsmbFile, VsmbFileSystem


def use_api(fs: VsmbFileSystem) -> None:
    fs.pipe({"/a": b"a", "/b": b"b"})
    data = fs.cat(["/a", "/b"])
    assert isinstance(data, dict)
    assert data["/a"] == b"a"
    handle: VsmbFile = fs.open("/a", "rb")
    assert handle.read() == b"a"
    handle.close()
    fs.close()
