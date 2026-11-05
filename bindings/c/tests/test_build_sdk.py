import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("build_sdk", Path(__file__).parents[1] / "build-sdk.py")
sdk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sdk)


class SdkTests(unittest.TestCase):
    def test_registry_check_allows_root_but_rejects_other_local_and_git_sources(self):
        root = {"id": "root", "name": "vfsi-c", "source": None}
        for source in (None, "git+https://example.invalid/repo", "registry+https://example.invalid/index"):
            metadata = {"resolve": {"root": "root", "nodes": [{"id": "root", "dependencies": ["dep"]}, {"id": "dep", "dependencies": []}]}, "packages": [
                root, {"id": "dep", "name": "vfsi-core", "source": source}]}
            if source and source.startswith("registry+"):
                sdk.verify_sources(metadata)
            else:
                with self.assertRaisesRegex(ValueError, "non-registry"):
                    sdk.verify_sources(metadata)

    def test_install_is_relocatable_and_records_the_actual_graph(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, output, prefix = root / "source", root / "output", root / "sdk"
            (source / "include").mkdir(parents=True)
            (source / "include" / "vfsi.h").write_text("header")
            (output / "vfsi-libs").mkdir(parents=True)
            (output / "vfsi-libs" / "libntirpc.so").write_bytes(b"native")
            name = "libvfsi_c.dylib" if sdk.sys.platform == "darwin" else "libvfsi_c.so"
            (output / name).write_bytes(b"library")
            paths = sdk.install(source, output, prefix, {"resolve": {"root": "sdk", "nodes": [{"id": "sdk", "dependencies": []}]}, "packages": [
                {"id": "sdk", "name": "vfsi-c", "version": "0.3.12"},
                {"id": "other", "name": "vnfs", "version": "999"}]})
            self.assertEqual(Path(paths["library"]).read_bytes(), b"library")
            self.assertEqual((prefix / "lib/vfsi-libs/libntirpc.so").read_bytes(), b"native")
            self.assertEqual((prefix / "include/vfsi.h").read_text(), "header")
            self.assertIn('"vfsi-c": "0.3.12"', (prefix / "versions.json").read_text())
            self.assertNotIn("vnfs", (prefix / "versions.json").read_text())


if __name__ == "__main__":
    unittest.main()
