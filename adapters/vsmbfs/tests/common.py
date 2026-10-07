"""Run the shared fsspec contract without importing the NFS adapter."""

import sys
from pathlib import Path

from vsmb import _native

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "test_support"))
from vfsi_test_support import run_correctness_suite as _run_suite  # noqa: E402


def run_correctness_suite(fs):
    _run_suite(fs, "vsmbfs", _native)
