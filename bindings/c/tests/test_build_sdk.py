import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

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
            notices = {"LICENSE-MIT": b"MIT notice\n", "LICENSE-APACHE": b"Apache notice\n"}
            for name, contents in notices.items():
                (source / name).write_bytes(contents)
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
            for name, contents in notices.items():
                self.assertEqual((prefix / name).read_bytes(), contents)
            self.assertIn('"vfsi-c": "0.3.12"', (prefix / "versions.json").read_text())
            self.assertNotIn("vnfs", (prefix / "versions.json").read_text())

    def test_build_installs_only_unique_cargo_reported_root_library(self):
        for outcome in ("shared", "static", "legacy", "missing", "ambiguous"):
            with self.subTest(outcome=outcome), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                source, target, prefix = root / "source", root / "target", root / "sdk"
                (source / "include").mkdir(parents=True)
                (source / "include/vfsi.h").write_text("header")
                name = "libvfsi_c.dylib" if sdk.sys.platform == "darwin" else "libvfsi_c.so"
                stale = target / "release" / name
                current = target / "configured-target" / "release" / name
                stale.parent.mkdir(parents=True)
                current.parent.mkdir(parents=True)
                stale.write_bytes(b"stale")
                current.write_bytes(b"current")
                profile_runtimes = current.parent / "vfsi-libs"
                profile_runtimes.mkdir()
                runtime_name = "libntirpc.dylib" if sdk.sys.platform == "darwin" else "libntirpc.so"
                (profile_runtimes / runtime_name).write_bytes(b"profile runtime")
                build_out = current.parent / "build/vfsi-c-current/out"
                build_out.mkdir(parents=True)
                if outcome in ("shared", "static"):
                    (build_out / "vfsi-libs").mkdir()
                    if outcome == "shared":
                        (build_out / "vfsi-libs" / runtime_name).write_bytes(b"current runtime")
                if outcome in ("shared", "static", "legacy"):
                    unrelated = prefix / "lib/vfsi-libs/unrelated"
                    unrelated.parent.mkdir(parents=True)
                    unrelated.write_bytes(b"preserved")
                metadata = {
                    "resolve": {"root": "root", "nodes": [{"id": "root", "dependencies": []}]},
                    "packages": [{"id": "root", "name": "vfsi-c", "version": "0.3.18", "source": None}],
                    "target_directory": str(target),
                }

                def cargo(command, **kwargs):
                    if command[1] == "metadata":
                        return json.dumps(metadata).encode()
                    artifact = lambda package, paths: json.dumps({
                        "reason": "compiler-artifact",
                        "package_id": package,
                        "target": {"crate_types": ["cdylib", "staticlib"]},
                        "filenames": [str(p) for p in paths],
                    })
                    messages = [
                        artifact("dependency", [stale]),
                        json.dumps({
                            "reason": "build-script-executed",
                            "package_id": "dependency",
                            "out_dir": str(stale.parent),
                        }),
                        json.dumps({
                            "reason": "build-script-executed",
                            "package_id": "root",
                            "out_dir": str(build_out),
                        }),
                    ]
                    if outcome != "missing":
                        messages.append(artifact("root", [current, current.with_suffix(".a")]))
                    if outcome == "ambiguous":
                        messages.append(artifact("root", [stale]))
                    return "\n".join(messages)

                with patch.object(sdk.sys, "argv", [
                    "build-sdk.py", "--manifest-path", str(source / "Cargo.toml"),
                    "--prefix", str(prefix), "--registry-only",
                ]), patch.object(sdk.subprocess, "check_output", side_effect=cargo), \
                     patch.object(sdk.subprocess, "run"):
                    if outcome in ("shared", "static", "legacy"):
                        sdk.main()
                        self.assertEqual((prefix / "lib" / name).read_bytes(), b"current")
                        self.assertEqual(unrelated.read_bytes(), b"preserved")
                        installed = prefix / "lib/vfsi-libs" / runtime_name
                        if outcome == "static":
                            self.assertFalse(installed.exists())
                        else:
                            expected = b"current runtime" if outcome == "shared" else b"profile runtime"
                            self.assertEqual(installed.read_bytes(), expected)
                    else:
                        with self.assertRaisesRegex(ValueError, "exactly one"):
                            sdk.main()
                        self.assertFalse(prefix.exists())


if __name__ == "__main__":
    unittest.main()
