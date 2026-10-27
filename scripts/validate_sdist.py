#!/usr/bin/env python3
"""Validate the single libdeno source archive used by release workflows."""

import argparse
import tarfile
import tomllib
from email.parser import Parser
from pathlib import Path
from zipfile import ZipFile

REQUIRED = {
    "pyproject.toml", "Cargo.toml", "Cargo.lock", "build.rs",
    "build_support/snapshot_arch.rs", "build_support/snapshot_bundle.rs",
    "build_support/snapshot_identity.rs", "build_support/snapshot_sources.rs",
    "src/lib.rs", "python/Cargo.toml",
    "python/Cargo.lock", "python/src/lib.rs", "python/libdeno/__init__.py",
    "README-pypi.md",
}


def _normalized(text: str) -> str:
    return text.replace("\r\n", "\n").rstrip("\n")


def validate(archives: list[Path], expected: str) -> None:
    if len(archives) != 1:
        raise ValueError(f"expected exactly one sdist, found {len(archives)}")
    archive_path = archives[0]
    if archive_path.name != f"libdeno-{expected}.tar.gz":
        raise ValueError(f"unexpected sdist filename: {archive_path.name}")
    with tarfile.open(archive_path, "r:gz") as archive:
        members = archive.getmembers()
        roots = {m.name.split("/", 1)[0] for m in members}
        root = f"libdeno-{expected}"
        if roots != {root}:
            raise ValueError(f"expected archive root {root}, found {sorted(roots)}")
        names = {m.name.removeprefix(root + "/") for m in members}
        missing = REQUIRED - names
        if missing:
            raise ValueError(f"sdist is missing required files: {sorted(missing)}")
        metadata = [m for m in members if m.name == f"{root}/PKG-INFO"]
        if len(metadata) != 1:
            raise ValueError("sdist must contain exactly one PKG-INFO")
        version = Parser().parsestr(archive.extractfile(metadata[0]).read().decode()).get_all("Version") or []
        if version != [expected]:
            raise ValueError(f"expected sdist version {expected}, got {version}")
        readme = archive.extractfile(f"{root}/README-pypi.md").read().decode()
        description = Parser().parsestr(archive.extractfile(metadata[0]).read().decode()).get_payload()
        expected_readme = (Path(__file__).resolve().parents[1] / "python/README-pypi.md").read_text()
        if _normalized(readme) != _normalized(expected_readme):
            raise ValueError("sdist README-pypi.md does not match python/README-pypi.md")
        if _normalized(readme) != _normalized(description):
            raise ValueError("sdist README-pypi.md does not match PKG-INFO description")
        for name, section in (("pyproject.toml", "project"), ("Cargo.toml", "package"), ("python/Cargo.toml", "package")):
            text = archive.extractfile(f"{root}/{name}").read().decode()
            config = tomllib.loads(text)
            found = config.get(section, {}).get("version")
            if found != expected:
                raise ValueError(f"{name}: expected version {expected}, got {found}")
            if name == "pyproject.toml" and config.get("project", {}).get("readme") != "README-pypi.md":
                raise ValueError("pyproject.toml must select README-pypi.md as project metadata")


def validate_wheel_readmes(archive_path: Path, wheels: list[Path]) -> None:
    with tarfile.open(archive_path, "r:gz") as source:
        root = source.getmembers()[0].name.split("/", 1)[0]
        readme = _normalized(source.extractfile(f"{root}/README-pypi.md").read().decode())
    for wheel in wheels:
        with ZipFile(wheel) as package:
            metadata = [name for name in package.namelist() if name.endswith(".dist-info/METADATA")]
            if len(metadata) != 1:
                raise ValueError(f"{wheel.name}: expected exactly one wheel METADATA file")
            description = _normalized(Parser().parsestr(package.read(metadata[0]).decode()).get_payload())
        if description != readme:
            raise ValueError(f"{wheel.name}: wheel description does not match Python README")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("expected_version")
    parser.add_argument("archives", nargs="+", type=Path)
    parser.add_argument("--wheels", nargs="*", type=Path, default=[])
    args = parser.parse_args()
    validate(args.archives, args.expected_version)
    if args.wheels:
        validate_wheel_readmes(args.archives[0], args.wheels)
        print(f"validated Python README in {len(args.wheels)} wheel METADATA files")
    print(f"validated libdeno sdist version {args.expected_version}")


if __name__ == "__main__":
    main()
