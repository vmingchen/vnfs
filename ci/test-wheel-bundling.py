#!/usr/bin/env python3
"""Regression tests for the static ntirpc wheel contract."""

import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location(
    "bundling", Path(__file__).with_name("check-wheel-bundling.py")
)
bundling = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bundling)


class WheelContractTests(unittest.TestCase):
    def make_wheel(self, extra=(), missing=()):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        wheel = Path(temp.name) / "nfs4fs.whl"
        with zipfile.ZipFile(wheel, "w") as archive:
            archive.writestr("nfs4fs/_native.abi3.so", b"ELF test fixture")
            for name in ("libgssapi_krb5.so.2", "liburcu-bp.so.8", *extra):
                if name not in missing:
                    archive.writestr(f"nfs4fs.libs/{name}", b"ELF test fixture")
        return wheel

    def test_static_ntirpc_with_bundled_dependencies_passes(self):
        with patch.object(
            bundling.subprocess,
            "run",
            return_value=subprocess.CompletedProcess([], 0, "(NEEDED) [libc.so.6]"),
        ):
            self.assertTrue(bundling.check(self.make_wheel()))

    def test_system_ntirpc_dependency_fails(self):
        with patch.object(
            bundling.subprocess,
            "run",
            return_value=subprocess.CompletedProcess(
                [], 0, "(NEEDED) [libntirpc.so.6]"
            ),
        ):
            self.assertFalse(bundling.check(self.make_wheel()))

    def test_bundled_second_ntirpc_fails(self):
        with patch.object(
            bundling.subprocess,
            "run",
            return_value=subprocess.CompletedProcess([], 0, "(NEEDED) [libc.so.6]"),
        ):
            self.assertFalse(
                bundling.check(self.make_wheel(extra=("libntirpc-hash.so.6",)))
            )

    def test_missing_runtime_dependency_fails(self):
        with patch.object(
            bundling.subprocess,
            "run",
            return_value=subprocess.CompletedProcess([], 0, "(NEEDED) [liburcu-bp.so.8]"),
        ):
            self.assertFalse(bundling.check(self.make_wheel(missing=("liburcu-bp.so.8",))))

    def test_static_build_without_dynamic_rcu_passes(self):
        with patch.object(
            bundling.subprocess,
            "run",
            return_value=subprocess.CompletedProcess([], 0, "(NEEDED) [libgssapi_krb5.so.2]"),
        ):
            self.assertTrue(bundling.check(self.make_wheel(missing=("liburcu-bp.so.8",))))

    def test_bundled_wrong_rcu_flavor_does_not_satisfy_needed_library(self):
        with patch.object(
            bundling.subprocess,
            "run",
            return_value=subprocess.CompletedProcess([], 0, "(NEEDED) [liburcu-cds.so.8]"),
        ):
            self.assertFalse(bundling.check(self.make_wheel()))

    def test_missing_gss_library_is_still_rejected(self):
        with patch.object(
            bundling.subprocess,
            "run",
            return_value=subprocess.CompletedProcess([], 0, "(NEEDED) [libc.so.6]"),
        ):
            self.assertFalse(bundling.check(self.make_wheel(missing=("libgssapi_krb5.so.2",))))

    def test_dependency_of_a_bundled_library_also_requires_rcu(self):
        with patch.object(
            bundling.subprocess,
            "run",
            side_effect=[
                subprocess.CompletedProcess([], 0, "(NEEDED) [libgssapi_krb5.so.2]"),
                subprocess.CompletedProcess([], 0, "(NEEDED) [liburcu-bp.so.8]"),
            ],
        ):
            self.assertFalse(bundling.check(self.make_wheel(missing=("liburcu-bp.so.8",))))

    def test_inspection_failure_is_not_accepted(self):
        with patch.object(
            bundling.subprocess,
            "run",
            side_effect=subprocess.CalledProcessError(1, ["readelf"]),
        ):
            with self.assertRaises(subprocess.CalledProcessError):
                bundling.check(self.make_wheel())


if __name__ == "__main__":
    unittest.main()
