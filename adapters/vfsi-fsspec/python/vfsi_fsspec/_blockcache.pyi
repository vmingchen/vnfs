from typing import Type

from fsspec.spec import AbstractFileSystem

def install_fsspec_blockcache_compat(fs_type: Type[AbstractFileSystem]) -> None: ...
