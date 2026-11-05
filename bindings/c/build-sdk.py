#!/usr/bin/env python3
"""Build/install a relocatable VFSI C SDK from its locked Cargo manifest."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


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


def install(source, output, prefix, metadata):
    include, library = prefix / "include", prefix / "lib"
    include.mkdir(parents=True, exist_ok=True)
    library.mkdir(parents=True, exist_ok=True)
    if not (source / "include/vfsi.h").is_file():
        raise ValueError("SDK source is missing include/vfsi.h")
    for header in (source / "include").glob("*.h"):
        shutil.copy2(header, include / header.name)
    name = "libvfsi_c.dylib" if sys.platform == "darwin" else "libvfsi_c.so"
    shutil.copy2(output / name, library / name)
    if (output / "vfsi-libs").is_dir():
        shutil.copytree(output / "vfsi-libs", library / "vfsi-libs", dirs_exist_ok=True)
    versions = {p["name"]: p["version"] for p in dependencies(metadata)
                if p["name"].startswith(("vfsi-", "vnfs", "libntirpc", "nfsv41"))}
    (prefix / "versions.json").write_text(json.dumps(versions, indent=2, sort_keys=True) + "\n")
    return {"include": str(include), "library": str(library / name)}


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
    subprocess.run(["cargo", "build", "--locked", "--manifest-path", str(manifest),
                    "--target-dir", str(target), "--profile", args.profile], check=True)
    output = target / ("debug" if args.profile == "dev" else "release")
    paths = install(manifest.parent, output, args.prefix.resolve(), metadata)
    for name, path in paths.items():
        print(f"{name}={path}")
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a") as stream:
            for name, path in paths.items():
                stream.write(f"{name}={path}\n")


if __name__ == "__main__":
    main()
