"""Compatibility import for the shared VFSI block-cache integration."""

from vfsi_fsspec._blockcache import install_fsspec_blockcache_compat

__all__ = ["install_fsspec_blockcache_compat"]
