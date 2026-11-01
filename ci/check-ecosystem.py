#!/usr/bin/env python3
"""Validate the VFSI external-component registry without network access."""

from __future__ import annotations

import pathlib
import re
import tomllib


ROOT = pathlib.Path(__file__).resolve().parents[1]
REGISTRY = ROOT / "ecosystem" / "ports.toml"
SHA = re.compile(r"[0-9a-f]{40}")
CANONICAL = "https://github.com/vmingchen/vnfs"
ALLOWED_KINDS = {"application-port", "test-infrastructure"}
ALLOWED_MATURITY = {"experimental", "validated", "maintained"}
REPOSITORY_PREFIXES = {
    "application-port": "https://github.com/vfsi/vfsi-port-",
    "test-infrastructure": "https://github.com/vfsi/vfsi-infra-",
}


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

        kind = component.get("kind")
        if kind not in ALLOWED_KINDS:
            raise SystemExit(f"{identifier}: invalid kind")
        if component.get("maturity") not in ALLOWED_MATURITY:
            raise SystemExit(f"{identifier}: invalid maturity")

        for field in ("revision", "upstream_base"):
            value = component.get(field, "")
            if not isinstance(value, str) or SHA.fullmatch(value) is None:
                raise SystemExit(f"{identifier}: {field} must be a full Git SHA")

        for field in ("repository", "upstream"):
            value = component.get(field, "")
            if not isinstance(value, str) or not value.startswith("https://github.com/"):
                raise SystemExit(f"{identifier}: {field} must be a GitHub HTTPS URL")

        repository = component["repository"]
        if not repository.startswith(REPOSITORY_PREFIXES[kind]):
            raise SystemExit(
                f"{identifier}: {kind} repository must use the "
                f"{REPOSITORY_PREFIXES[kind]} prefix"
            )

        planned_repository = component.get("planned_repository")
        if planned_repository is not None:
            if not isinstance(planned_repository, str) or not planned_repository.startswith(
                REPOSITORY_PREFIXES[kind]
            ):
                raise SystemExit(
                    f"{identifier}: planned_repository must use the "
                    f"{REPOSITORY_PREFIXES[kind]} prefix"
                )

        test_path = ROOT / component.get("compatibility_test", "")
        if not test_path.is_file():
            raise SystemExit(f"{identifier}: missing compatibility test {test_path}")

        abi = component.get("vfsi_c_abi")
        if not isinstance(abi, int) or abi < 0:
            raise SystemExit(f"{identifier}: vfsi_c_abi must be a non-negative integer")

    ports = {c['id']: c['revision'] for c in components if c['kind'] == 'application-port'}
    if set(ports) != {'coreutils', 'findutils', 'git', 'rsync'}:
        raise SystemExit('register all four current application ports')
    baseline = tomllib.loads((ROOT / 'ecosystem/versions.toml').read_text())
    if baseline.get('schema_version') != 1 or baseline['platform']['canonical'] != CANONICAL:
        raise SystemExit('invalid baseline schema or canonical repository')
    if baseline['platform']['mirror'] != 'https://github.com/vfsi/vfsi':
        raise SystemExit('invalid platform mirror')
    if baseline['ports'] != ports:
        raise SystemExit('version manifest port revisions differ from the registry')
    versions = {'rust': {}, 'python': {}}
    for path in [*ROOT.glob('crates/*/Cargo.toml'), ROOT / 'bindings/c/Cargo.toml']:
        package = tomllib.loads(path.read_text())['package']
        if package.get('publish') is not False:
            section = baseline['c'] if package['name'] == 'vfsi-c' else baseline['rust'][package['name']]
            if tuple(map(int, section['version'].split('.'))) > tuple(map(int, package['version'].split('.'))):
                raise SystemExit(f"{package['name']}: baseline version exceeds source")
            if package['name'] != 'vfsi-c':
                versions['rust'][package['name']] = package['version']
    for path in ROOT.glob('adapters/*/pyproject.toml'):
        package = tomllib.loads(path.read_text()).get('project', {})
        if 'version' in package:
            versions['python'][package['name']] = package['version']
    for language, expected in versions.items():
        if set(baseline[language]) != set(expected):
            raise SystemExit(f'baseline {language} packages differ from manifests')
        for name, version in expected.items():
            if tuple(map(int, baseline[language][name]['version'].split('.'))) > tuple(map(int, version.split('.'))):
                raise SystemExit(f'{name}: baseline version exceeds source')
    revisions = [baseline['platform']['code_revision'], baseline['c']['revision'],
                 baseline['c']['scoped_identity_revision'],
                 *(p['revision'] for group in versions for p in baseline[group].values())]
    if any(SHA.fullmatch(rev) is None for rev in revisions):
        raise SystemExit('baseline revisions must be full Git SHAs')
    if baseline['c']['package'] != 'vfsi-c' or baseline['c']['abi'] != 3:
        raise SystemExit('invalid C package/ABI baseline')
    print(f"validated {len(components)} ecosystem components and package baseline")


if __name__ == "__main__":
    main()
