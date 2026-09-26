#!/usr/bin/env python3
"""Enforce VFSI package identity and protocol ownership in registry metadata."""

import sys
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parents[1]

PUBLIC_RUST_PACKAGES = (
    "crates/vnfs/Cargo.toml",
    "crates/nfsv41-sys/Cargo.toml",
    "crates/vfsi-core/Cargo.toml",
    "crates/vfsi-sync/Cargo.toml",
    "crates/vfsi-nfs/Cargo.toml",
    "crates/vfsi-smb/Cargo.toml",
    "crates/vfsi-local/Cargo.toml",
    "bindings/c/Cargo.toml",
)

PRIVATE_RUST_PACKAGES = (
    "adapters/nfs4fs/Cargo.toml",
    "adapters/vfsi-python/Cargo.toml",
    "adapters/vsmb/Cargo.toml",
)

PYTHON_PACKAGES = (
    "adapters/nfs4fs/pyproject.toml",
    "adapters/vfsi-fsspec/pyproject.toml",
    "adapters/vsmb/pyproject.toml",
    "adapters/vsmbfs/pyproject.toml",
)


def load(relative_path: str) -> dict:
    with (ROOT / relative_path).open("rb") as manifest:
        return tomllib.load(manifest)


def main() -> int:
    errors: list[str] = []
    for relative_path in PUBLIC_RUST_PACKAGES + PRIVATE_RUST_PACKAGES:
        package = load(relative_path)["package"]
        keywords = {keyword.lower() for keyword in package.get("keywords", [])}
        if "vfsi" not in keywords:
            errors.append(f"{relative_path}: [package].keywords must include 'vfsi'")

    for relative_path in PUBLIC_RUST_PACKAGES:
        package = load(relative_path)["package"]
        if package.get("publish") is False:
            errors.append(f"{relative_path}: public Rust package must be publishable")

    for relative_path in PRIVATE_RUST_PACKAGES:
        package = load(relative_path)["package"]
        if package.get("publish") is not False:
            errors.append(
                f"{relative_path}: native build crate must set publish = false"
            )

    for relative_path in PYTHON_PACKAGES:
        project = load(relative_path)["project"]
        keywords = {keyword.lower() for keyword in project.get("keywords", [])}
        if "vfsi" not in keywords:
            errors.append(f"{relative_path}: [project].keywords must include 'vfsi'")

    nfs4fs = load("adapters/nfs4fs/pyproject.toml")["project"]
    engine = load("adapters/vfsi-fsspec/pyproject.toml")["project"]
    nfs4fs_native = load("adapters/nfs4fs/Cargo.toml")["package"]
    if nfs4fs["version"] != nfs4fs_native["version"]:
        errors.append("nfs4fs: Python and native package versions must match")
    engine_requirements = [
        dependency
        for dependency in nfs4fs["dependencies"]
        if dependency.startswith("vfsi-fsspec>=")
    ]
    expected = f"vfsi-fsspec>={engine['version']},<0.2"
    if engine_requirements != [expected]:
        errors.append(f"nfs4fs: require the current shared engine with {expected!r}")

    vnfs = load("crates/vnfs/Cargo.toml")
    package = vnfs["package"]
    if "smb" in package["description"].lower():
        errors.append("crates/vnfs: description must remain NFS-focused")
    if "smb" in {keyword.lower() for keyword in package.get("keywords", [])}:
        errors.append("crates/vnfs: the 'smb' keyword belongs to vfsi-smb")
    if "smb" in vnfs.get("features", {}):
        errors.append("crates/vnfs: the SMB backend belongs to vfsi-smb")
    if "vfsi-smb" in vnfs.get("dependencies", {}):
        errors.append("crates/vnfs: must not depend on the vfsi-smb backend")

    if errors:
        print("package metadata validation failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print("package metadata is consistent with the VFSI organization")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
