#!/usr/bin/env python3
"""Check checked-in Deno/TypeScript declarations without network access."""

import re
import json
import subprocess
import tomllib
from pathlib import Path


ROOT = Path(__file__).parents[2]


def exactly_one(pattern: str, text: str, label: str) -> str:
    matches = re.findall(pattern, text, re.MULTILINE)
    if len(matches) != 1:
        raise SystemExit(
            f"manual upstream check required: {label} has "
            f"{len(matches)} local declarations"
        )
    return matches[0]


manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
upstream = (
    manifest.get("package", {})
    .get("metadata", {})
    .get("libdeno", {})
    .get("upstream")
)
required = {
    "deno-version",
    "typescript-version",
    "deno-runtime-version",
    "deno-core-version",
    "deno-resolver-version",
    "deno-graph-version",
}
if not isinstance(upstream, dict) or not required <= upstream.keys():
    raise SystemExit(
        "manual upstream check required: Cargo.toml must contain the complete "
        "[package.metadata.libdeno.upstream] ledger"
    )
for key in required:
    value = upstream[key]
    if not isinstance(value, str) or not re.fullmatch(r"\d+\.\d+\.\d+", value):
        raise SystemExit(f"manual upstream check required: invalid {key} = {value!r}")

deno = exactly_one(
    r'^\s*const\s+DENO_VERSION\s*:\s*&str\s*=\s*"([^"]+)"\s*;',
    (ROOT / "src/deno_runtime_adapter.rs").read_text(encoding="utf-8"),
    "src/deno_runtime_adapter.rs DENO_VERSION",
)
ts = exactly_one(
    r'^\s*const\s+TS_VERSION\s*:\s*&str\s*=\s*"([^"]+)"\s*;',
    (ROOT / "build.rs").read_text(encoding="utf-8"),
    "build.rs TS_VERSION",
)
if deno != upstream["deno-version"]:
    raise SystemExit(
        f"Deno version drift: source {deno!r}, ledger {upstream['deno-version']!r}"
    )
if ts != upstream["typescript-version"]:
    raise SystemExit(
        f"TypeScript version drift: build.rs {ts!r}, "
        f"ledger {upstream['typescript-version']!r}"
    )


def declared(section: str, name: str):
    value = manifest.get(section, {}).get(name)
    if isinstance(value, str):
        return value
    if isinstance(value, dict):
        return value.get("version")
    return None


dependency_checks = {
    "deno-runtime-version": [
        ("dependencies", "deno_runtime"),
        ("build-dependencies", "deno_runtime"),
    ],
    "deno-core-version": [
        ("dependencies", "deno_core"),
        ("build-dependencies", "deno_core"),
    ],
    "deno-resolver-version": [("dependencies", "deno_resolver")],
    "deno-graph-version": [("dependencies", "deno_graph")],
}
for ledger_key, locations in dependency_checks.items():
    for section, name in locations:
        value = declared(section, name)
        if value is None or value.lstrip("=") != upstream[ledger_key]:
            raise SystemExit(
                f"dependency version drift: {section}.{name} declares {value!r}, "
                f"ledger requires {upstream[ledger_key]!r}"
            )


def cargo_metadata(manifest_path: Path):
    # --no-deps omits the resolve graph, so use the full graph without allowing
    # Cargo to update or fetch anything.
    try:
        result = subprocess.run(
            [
                "cargo",
                "metadata",
                "--locked",
                "--offline",
                "--format-version",
                "1",
                "--manifest-path",
                str(manifest_path),
            ],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        )
        return json.loads(result.stdout)
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        raise SystemExit(f"cargo metadata check failed for {manifest_path}: {error}")


def metadata_root(metadata, manifest_path: Path):
    matches = [
        package
        for package in metadata.get("packages", [])
        if Path(package.get("manifest_path", "")).resolve() == manifest_path.resolve()
    ]
    if len(matches) != 1:
        raise SystemExit(
            f"cargo metadata check failed for {manifest_path}: "
            f"found {len(matches)} root packages"
        )
    package = matches[0]
    nodes = {node["id"]: node for node in metadata.get("resolve", {}).get("nodes", [])}
    node = nodes.get(package.get("id"))
    if node is None:
        raise SystemExit(
            f"cargo metadata check failed for {manifest_path}: root resolve node missing"
        )
    packages = {item["id"]: item for item in metadata.get("packages", [])}
    return node, packages


def resolved_direct_package(metadata, manifest_path: Path, name: str):
    node, packages = metadata_root(metadata, manifest_path)
    edges = [edge for edge in node.get("deps", []) if edge.get("name") == name]
    if len(edges) != 1:
        raise SystemExit(
            f"{manifest_path} metadata drift: root dependency {name} has "
            f"{len(edges)} resolved edges"
        )
    package = packages.get(edges[0].get("pkg"))
    if package is None:
        raise SystemExit(
            f"{manifest_path} metadata drift: root dependency {name} "
            "points to an unknown package"
        )
    if package.get("name") != name:
        raise SystemExit(
            f"{manifest_path} metadata drift: root dependency {name} "
            f"resolves to package {package.get('name')!r}"
        )
    return package


def resolved_direct_version(metadata, manifest_path: Path, name: str) -> str:
    return resolved_direct_package(metadata, manifest_path, name)["version"]


root_manifest = ROOT / "Cargo.toml"
root_metadata = cargo_metadata(root_manifest)
root_node, _ = metadata_root(root_metadata, root_manifest)
for ledger_key, locations in dependency_checks.items():
    name = locations[0][1]
    resolved = resolved_direct_version(root_metadata, root_manifest, name)
    if resolved != upstream[ledger_key]:
        raise SystemExit(
            f"Cargo metadata drift: root dependency {name} resolves to {resolved!r}, "
            f"ledger requires {upstream[ledger_key]!r}"
        )

python_manifest = ROOT / "python/Cargo.toml"
python_metadata = cargo_metadata(python_manifest)
python_package = resolved_direct_package(python_metadata, python_manifest, "libdeno")
if (
    python_package["id"] != root_node["id"]
    or python_package["version"] != manifest["package"]["version"]
):
    raise SystemExit(
        f"Cargo metadata drift: {python_manifest} libdeno resolves to "
        f"{python_package['id']}@{python_package['version']}, "
        f"root package requires {root_node['id']}@{manifest['package']['version']}"
    )

for lock_path in (ROOT / "Cargo.lock", ROOT / "python/Cargo.lock"):
    lock = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    for name, ledger_key in (
        ("deno_runtime", "deno-runtime-version"),
        ("deno_core", "deno-core-version"),
        ("deno_resolver", "deno-resolver-version"),
        ("deno_graph", "deno-graph-version"),
    ):
        locked = {
            package["version"]
            for package in lock["package"]
            if package["name"] == name
        }
        if upstream[ledger_key] not in locked:
            raise SystemExit(
                f"{lock_path} drift: {name} has {sorted(locked)!r}, "
                f"ledger requires {upstream[ledger_key]!r}"
            )

print(
    "offline local version consistency passed; upstream Deno/TypeScript provenance "
    "still requires manual inspection of the matching Deno tag "
    "(no network lookup performed)"
)
