"""Static-use checks for vfsi-fsspec's installed typing surface."""

from os import PathLike
from pathlib import Path
from typing import Mapping, Optional, Union

from vfsi_fsspec import VfsiFile, VfsiFileSystem, install_fsspec_blockcache_compat


def accepts_filesystem_type(filesystem: type[VfsiFileSystem]) -> None:
    install_fsspec_blockcache_compat(target_type=filesystem)


accepts_filesystem_type(VfsiFileSystem)
filesystem_type: type[VfsiFileSystem] = VfsiFileSystem
file_type: type[VfsiFile] = VfsiFile


def range_inputs(fs: VfsiFileSystem) -> None:
    names: list[str] = ["/a"]
    paths: list[Path] = [Path("/a")]
    mixed: list[Union[str, Path]] = ["/a", Path("/b")]
    starts: list[int] = [0]
    ends: list[int] = [2]
    optional_ends: list[Optional[int]] = [None]
    fs.cat_ranges(names, starts, ends)
    optional_starts: list[Optional[int]] = [None]
    fs.cat_ranges(names, optional_starts, ends)
    fs.cat_ranges(names, None, ends)
    fs.cat_ranges(paths, 0, optional_ends)
    fs.cat_ranges(mixed, 0, None)
    # Strict mode rejects unused ignores, pinning the runtime's list-only contract.
    fs.cat_ranges(("/a",), starts, ends)  # type: ignore[arg-type]
    fs.cat_ranges(names, (0,), ends)  # type: ignore[arg-type]
    fs.cat_ranges(names, starts, (2,))  # type: ignore[arg-type]


def pipe_inputs(fs: VfsiFileSystem) -> None:
    names: dict[str, bytes] = {"/a": b"a"}
    paths: dict[Path, bytearray] = {Path("/b"): bytearray(b"b")}
    mixed: dict[Union[str, Path], Union[bytes, bytearray, memoryview]] = {
        "/a": b"a",
        Path("/b"): memoryview(b"b"),
    }
    fs.pipe(names)
    fs.pipe(paths)
    fs.pipe(mixed)
    generic: Mapping[Union[str, PathLike[str]], Union[bytes, bytearray, memoryview]] = {
        "/a": b"a"
    }
    fs.pipe(generic)  # type: ignore[arg-type]


def buffered_file_methods(file: VfsiFile) -> bytes:
    file.flush(force=True)
    file.read(None)
    return file.readuntil(bytearray(b"END"), blocks=8)
