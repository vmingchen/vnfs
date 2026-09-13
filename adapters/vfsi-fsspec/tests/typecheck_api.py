"""Static-use checks for vfsi-fsspec's installed typing surface."""

from vfsi_fsspec import VfsiFile, VfsiFileSystem, install_fsspec_blockcache_compat


def accepts_filesystem_type(filesystem: type[VfsiFileSystem]) -> None:
    install_fsspec_blockcache_compat(filesystem)


accepts_filesystem_type(VfsiFileSystem)
filesystem_type: type[VfsiFileSystem] = VfsiFileSystem
file_type: type[VfsiFile] = VfsiFile
