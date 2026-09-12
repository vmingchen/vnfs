#!/usr/bin/env python3
"""Validate the VFSI external-component registry without network access."""

from __future__ import annotations

import pathlib
import re
import tomllib


ROOT = pathlib.Path(__file__).resolve().parents[1]
REGISTRY = ROOT / "ecosystem" / "ports.toml"
SHA = re.compile(r"[0-9a-f]{40}")
ALLOWED_KINDS = {"application-port", "test-infrastructure"}
ALLOWED_MATURITY = {"experimental", "validated", "maintained"}


def main() -> None:
    data = tomllib.loads(REGISTRY.read_text())
    if data.get("schema_version") != 1:
        raise SystemExit("ecosystem registry must use schema_version = 1")

    components = data.get("component", [])
    if not components:
        raise SystemExit("ecosystem registry must contain components")

    seen: set[str] = set()
    for component in components:
        identifier = component.get("id")
        if not isinstance(identifier, str) or not identifier:
            raise SystemExit("every component needs a non-empty id")
        if identifier in seen:
            raise SystemExit(f"duplicate component id: {identifier}")
        seen.add(identifier)

        if component.get("kind") not in ALLOWED_KINDS:
            raise SystemExit(f"{identifier}: invalid kind")
        if component.get("maturity") not in ALLOWED_MATURITY:
            raise SystemExit(f"{identifier}: invalid maturity")

        for field in ("revision", "upstream_base"):
            value = component.get(field, "")
            if not isinstance(value, str) or SHA.fullmatch(value) is None:
                raise SystemExit(f"{identifier}: {field} must be a full Git SHA")

        for field in ("repository", "planned_repository", "upstream"):
            value = component.get(field, "")
            if not isinstance(value, str) or not value.startswith("https://github.com/"):
                raise SystemExit(f"{identifier}: {field} must be a GitHub HTTPS URL")

        test_path = ROOT / component.get("compatibility_test", "")
        if not test_path.is_file():
            raise SystemExit(f"{identifier}: missing compatibility test {test_path}")

        abi = component.get("vfsi_c_abi")
        if not isinstance(abi, int) or abi < 0:
            raise SystemExit(f"{identifier}: vfsi_c_abi must be a non-negative integer")

    print(f"validated {len(components)} ecosystem components")


if __name__ == "__main__":
    main()
