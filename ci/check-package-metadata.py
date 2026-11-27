#!/usr/bin/env python3
"""Validate VFSI registry metadata and coordinated dependency floors."""

import re
import sys
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parents[1]
CANONICAL = "https://github.com/vmingchen/vnfs"

PUBLIC_RUST_PACKAGES = (
    "crates/vnfs/Cargo.toml",
    "crates/nfsv41-sys/Cargo.toml",
    "crates/vfsi-core/Cargo.toml",
    "crates/vfsi-sync/Cargo.toml",
    "crates/vfsi-nfs/Cargo.toml",
    "crates/vfsi-smb/Cargo.toml",
    "crates/vfsi-local/Cargo.toml",
    "crates/vfsi-posix/Cargo.toml",
    "crates/vfsi-uring/Cargo.toml",
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


def dependency_name(requirement: str) -> str:
    # PEP 508 puts the distribution name before extras and version constraints.
    name = re.split(r"[^a-zA-Z0-9_.-]", requirement.lstrip(), maxsplit=1)[0]
    return re.sub(r"[-_.]+", "-", name).lower()


def main() -> int:
    errors: list[str] = []

    # Cargo's workspace build uses the path, while a packaged consumer uses
    # this version requirement. Keep the coordinated dependency floors in sync.
    for name, dependency in load("Cargo.toml")["workspace"]["dependencies"].items():
        if not isinstance(dependency, dict) or "path" not in dependency:
            continue
        version = load(f"{dependency['path']}/Cargo.toml")["package"]["version"]
        if dependency.get("version") != version:
            errors.append(f"Cargo.toml: {name} dependency floor must match {version}")

    for relative_path in PUBLIC_RUST_PACKAGES + PRIVATE_RUST_PACKAGES:
        package = load(relative_path)["package"]
        if package.get('repository') != CANONICAL:
            errors.append(f'{relative_path}: repository must remain {CANONICAL}')
        keywords = {keyword.lower() for keyword in package.get("keywords", [])}
        if "vfsi" not in keywords:
            errors.append(f"{relative_path}: [package].keywords must include 'vfsi'")

    for relative_path in PUBLIC_RUST_PACKAGES:
        package = load(relative_path)["package"]
        if package.get("publish") is False:
            errors.append(f"{relative_path}: public Rust package must be publishable")

    native_artifacts: dict[str, str] = {}
    for relative_path in PRIVATE_RUST_PACKAGES:
        manifest = load(relative_path)
        package = manifest["package"]
        if package.get("publish") is not False:
            errors.append(
                f"{relative_path}: native build crate must set publish = false"
            )
        artifact = manifest.get("lib", {}).get(
            "name", package["name"].replace("-", "_")
        )
        if artifact in native_artifacts:
            errors.append(
                f"{relative_path}: native artifact {artifact!r} collides with "
                f"{native_artifacts[artifact]} in a shared Cargo target directory"
            )
        native_artifacts[artifact] = relative_path

    projects = {}
    for relative_path in PYTHON_PACKAGES:
        project = load(relative_path)["project"]
        projects[Path(relative_path).parent.name] = project
        for name, url in project.get('urls', {}).items():
            if name.lower() in {'repository', 'source', 'issues', 'changelog'}:
                if url != CANONICAL and not url.startswith(CANONICAL + '/'):
                    errors.append(f'{relative_path}: {name} must point to the canonical repository')
        keywords = {keyword.lower() for keyword in project.get("keywords", [])}
        if "vfsi" not in keywords:
            errors.append(f"{relative_path}: [project].keywords must include 'vfsi'")

    for package in ("nfs4fs", "vsmb"):
        native = load(f"adapters/{package}/Cargo.toml")["package"]
        if projects[package]["version"] != native["version"]:
            errors.append(f"{package}: Python and native package versions must match")

    for adapter, dependencies in (
        ("nfs4fs", ("vfsi-fsspec",)),
        ("vsmbfs", ("vfsi-fsspec", "vsmb")),
    ):
        for dependency in dependencies:
            requirements = [
                requirement
                for requirement in projects[adapter]["dependencies"]
                if dependency_name(requirement) == dependency
            ]
            expected = f"{dependency}>={projects[dependency]['version']},<0.2"
            if requirements != [expected]:
                errors.append(f"{adapter}: require the current {dependency} with {expected!r}")

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
