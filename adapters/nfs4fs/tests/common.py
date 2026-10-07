"""Run the shared fsspec contract with the NFS adapter installed independently."""

import sys
from pathlib import Path

from nfs4fs import _native

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "test_support"))
from vfsi_test_support import run_correctness_suite as _run_suite  # noqa: E402


def run_correctness_suite(fs):
    _run_suite(fs, "nfs4", _native)
