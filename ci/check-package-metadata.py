#!/usr/bin/env python3
"""Enforce VFSI package identity and protocol ownership in registry metadata."""

from pathlib import Path
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]

RUST_PACKAGES = (
    "crates/vnfs/Cargo.toml",
    "crates/nfsv41-sys/Cargo.toml",
    "crates/vfsi-core/Cargo.toml",
    "crates/vfsi-sync/Cargo.toml",
    "crates/vfsi-nfs/Cargo.toml",
    "crates/vfsi-smb/Cargo.toml",
    "crates/vfsi-local/Cargo.toml",
    "bindings/c/Cargo.toml",
    "adapters/nfs4fs/Cargo.toml",
    "adapters/vfsi-python/Cargo.toml",
    "adapters/vsmb/Cargo.toml",
    "testing/vfsi-test-support/Cargo.toml",
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
    for relative_path in RUST_PACKAGES:
        package = load(relative_path)["package"]
        keywords = {keyword.lower() for keyword in package.get("keywords", [])}
        if "vfsi" not in keywords:
            errors.append(f"{relative_path}: [package].keywords must include 'vfsi'")

    for relative_path in PYTHON_PACKAGES:
        project = load(relative_path)["project"]
        keywords = {keyword.lower() for keyword in project.get("keywords", [])}
        if "vfsi" not in keywords:
            errors.append(f"{relative_path}: [project].keywords must include 'vfsi'")

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
