#!/usr/bin/env python3
"""Build/install a relocatable VFSI C SDK from its locked Cargo manifest."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


LIBRARY_NAME = "libvfsi_c.dylib" if sys.platform == "darwin" else "libvfsi_c.so"

def dependencies(metadata):
    root = metadata["resolve"]["root"]
    if root is None:
        raise ValueError("SDK manifest must identify one root package")
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    pending, reachable = [root], set()
    while pending:
        key = pending.pop()
        if key in reachable:
            continue
        reachable.add(key)
        pending.extend(nodes[key]["dependencies"])
    return [package for package in metadata["packages"] if package["id"] in reachable]


def verify_sources(metadata):
    root = metadata["resolve"]["root"]
    if root is None:
        raise ValueError("SDK manifest must identify one root package")
    for package in dependencies(metadata):
        source = package["source"]
        if package["id"] != root and (source is None or not source.startswith("registry+")):
            raise ValueError(f"non-registry dependency: {package['name']} {source}")


def _replace_library(source, destination):
    destination = Path(destination)
    # Publish a new inode so processes with the previous library mapped
    # can continue running while new loads see the complete replacement.
    with tempfile.TemporaryDirectory(
        prefix=".vfsi-install-", dir=destination.parent
    ) as temporary:
        staged = Path(temporary) / destination.name
        shutil.copy2(source, staged)
        os.replace(staged, destination)
    return str(destination)


def install(source, output, prefix, metadata, runtime_dir=None):
    include, library = prefix / "include", prefix / "lib"
    include.mkdir(parents=True, exist_ok=True)
    library.mkdir(parents=True, exist_ok=True)
    if not (source / "include/vfsi.h").is_file():
        raise ValueError("SDK source is missing include/vfsi.h")
    for header in (source / "include").glob("*.h"):
        shutil.copy2(header, include / header.name)
    for name in ("LICENSE-MIT", "LICENSE-APACHE"):
        notice = source / name
        if notice.is_file():
            shutil.copy2(notice, prefix / name)
    if runtime_dir is None:
        runtime_dir = output / "vfsi-libs"
    if runtime_dir.is_dir() and any(runtime_dir.iterdir()):
        shutil.copytree(
            runtime_dir, library / "vfsi-libs", dirs_exist_ok=True,
            copy_function=_replace_library,
        )
    _replace_library(output / LIBRARY_NAME, library / LIBRARY_NAME)
    versions = {p["name"]: p["version"] for p in dependencies(metadata)
                if p["name"].startswith(("vfsi-", "vnfs", "libntirpc", "nfsv41"))}
    (prefix / "versions.json").write_text(json.dumps(versions, indent=2, sort_keys=True) + "\n")
    return {"include": str(include), "library": str(library / LIBRARY_NAME)}


def build_output(manifest, target, profile, metadata):
    root = metadata["resolve"]["root"]
    if root is None:
        raise ValueError("SDK manifest must identify one root package")
    messages = subprocess.check_output([
        "cargo", "build", "--locked", "--manifest-path", str(manifest),
        "--target-dir", str(target), "--profile", profile,
        "--message-format=json-render-diagnostics",
    ], text=True)
    artifacts = set()
    build_outputs = set()
    for line in messages.splitlines():
        message = json.loads(line)
        if message.get("package_id") != root:
            continue
        if message["reason"] == "build-script-executed":
            build_outputs.add(Path(message["out_dir"]).resolve())
        if (message["reason"] != "compiler-artifact"
                or "cdylib" not in message["target"]["crate_types"]):
            continue
        for filename in message["filenames"]:
            artifact = Path(filename)
            if artifact.name == LIBRARY_NAME:
                artifacts.add(artifact.resolve())
    if len(artifacts) != 1:
        raise ValueError(f"Cargo must report exactly one root {LIBRARY_NAME} artifact")
    if len(build_outputs) > 1:
        raise ValueError("Cargo must report at most one root build-script output")
    runtime_dir = None
    if build_outputs:
        candidate = build_outputs.pop() / "vfsi-libs"
        if candidate.is_dir():
            runtime_dir = candidate
    return artifacts.pop().parent, runtime_dir


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest-path", type=Path, default=Path(__file__).resolve().with_name("Cargo.toml"))
    parser.add_argument("--target-dir", type=Path)
    parser.add_argument("--prefix", type=Path, required=True)
    parser.add_argument("--profile", choices=("dev", "release"), default="release")
    parser.add_argument("--registry-only", action="store_true",
                        help="reject local/git dependencies before building; use for published SDKs")
    args = parser.parse_args()
    manifest = args.manifest_path.resolve()
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--locked", "--format-version", "1", "--manifest-path", str(manifest)]))
    if args.registry_only:
        verify_sources(metadata)
    target = (args.target_dir or Path(metadata["target_directory"])).resolve()
    output, runtime_dir = build_output(manifest, target, args.profile, metadata)
    paths = install(manifest.parent, output, args.prefix.resolve(), metadata, runtime_dir)
    for name, path in paths.items():
        print(f"{name}={path}")
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a") as stream:
            for name, path in paths.items():
                stream.write(f"{name}={path}\n")


if __name__ == "__main__":
    main()
