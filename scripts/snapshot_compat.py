#!/usr/bin/env python3
"""Run Cargo with a content identity for libdeno's snapshot dependencies."""

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

ROOT = Path(__file__).parents[1]
SIDECAR = "libdeno-snapshot.sources"
VCS_DIRS = {".git", ".hg", ".svn"}
CARGO_METADATA = {".cargo-ok", ".cargo-checksum.json", ".cargo_vcs_info.json"}
TRANSPORT_FEATURES = {"deno_runtime": {"snapshot"}}


def put(hash_, data):
    hash_.update(len(data).to_bytes(8, "big"))
    hash_.update(data)


def package_hash(package, features):
    """Hash regular files; only root target output, VCS dirs, and exact Cargo metadata are skipped."""
    root = Path(package["manifest_path"]).parent
    digest = hashlib.sha256()
    for value in (package["name"], package["version"], source_kind(package), *sorted(features)):
        put(digest, value.encode())
    files = []

    def visit(directory, is_root=False):
        for entry in sorted(os.scandir(directory), key=lambda item: item.name):
            if entry.name in VCS_DIRS or (is_root and entry.name in CARGO_METADATA):
                continue
            if is_root and entry.name == "target":
                continue
            path = Path(entry.path)
            if entry.is_symlink():
                raise ValueError(f"symlink in dependency source tree: {path}")
            if entry.is_dir():
                visit(path)
            elif entry.is_file():
                files.append(path)

    visit(root, is_root=True)
    for path in sorted(files, key=lambda path: path.relative_to(root).parts):
        put(digest, path.relative_to(root).as_posix().encode())
        put(digest, path.read_bytes())
    return digest.hexdigest()


def source_kind(package):
    source = package.get("source") or "path"
    return source.split("+", 1)[0]


def consumer_package(metadata, manifest=None):
    root = metadata.get("resolve", {}).get("root")
    if manifest is not None:
        packages = metadata["packages"]
        path = Path(manifest).resolve()
        matches = [p["id"] for p in packages
                   if Path(p["manifest_path"]).resolve() == path]
        if len(matches) != 1:
            raise ValueError("manifest must identify exactly one Cargo package; use a member manifest, not a virtual workspace")
        root = matches[0]
        workspace_manifest = Path(metadata["workspace_root"]).resolve() / "Cargo.toml"
        if path == workspace_manifest.resolve() and set(metadata["workspace_default_members"]) != {root}:
            raise ValueError("workspace default members differ from this manifest; use an explicit member manifest")
    return root


def graph_identity(metadata, manifest=None):
    """Hash the Deno/engine snapshot family, not the consumer's full native ABI closure."""
    packages = {package["id"]: package for package in metadata["packages"]}
    root = consumer_package(metadata, manifest)
    if root not in packages:
        raise ValueError("Cargo metadata has no single consumer package root; use its package manifest")
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    def reachable(start):
        found, pending = set(), [start]
        while pending:
            package_id = pending.pop()
            if package_id in found:
                continue
            found.add(package_id)
            for dep in nodes.get(package_id, {}).get("deps", []):
                kinds = dep.get("dep_kinds", [])
                # Snapshot inputs are runtime/build dependencies, never dev-only edges.
                if any(kind.get("kind") in (None, "build") for kind in kinds):
                    pending.append(dep.get("pkg", dep.get("id")))
        return found

    consumer_graph = reachable(root)
    libdeno = [package_id for package_id in consumer_graph
               if packages[package_id]["name"] == "libdeno"]
    if len(libdeno) != 1:
        raise ValueError(f"expected exactly one reachable libdeno package, found {len(libdeno)}")
    libdeno_graph = reachable(libdeno[0])
    roots = {}
    for name in ("deno_core", "deno_runtime"):
        matches = [pid for pid in libdeno_graph if packages[pid]["name"].replace("-", "_") == name]
        if len(matches) != 1:
            raise ValueError(f"expected exactly one reachable {name} package, found {len(matches)}")
        roots[name] = matches[0]
    snapshot_graph = reachable(roots["deno_core"]) | reachable(roots["deno_runtime"])
    selected = {}
    for package_id in snapshot_graph:
        package = packages[package_id]
        name = package["name"].replace("-", "_")
        if name.startswith("deno_") or name in {"v8", "serde_v8"}:
            if name in selected:
                raise ValueError(f"multiple reachable {name} packages; snapshot identity is ambiguous")
            selected[name] = package
    missing = {"deno_core", "deno_runtime", "deno_node"} - selected.keys()
    if missing:
        raise ValueError(f"Cargo dependency graph does not reach required packages: {sorted(missing)}")
    digest = hashlib.sha256()
    for name in sorted(selected):
        put(digest, name.encode())
        package_id = selected[name]["id"]
        features = set(nodes.get(package_id, {}).get("features", []))
        # This exposes deno_runtime::snapshot for producing snapshots in the
        # build script; import-only builds consume the serialized snapshot.
        features.difference_update(TRANSPORT_FEATURES.get(name, set()))
        put(digest, package_hash(selected[name], features).encode())
    return digest.hexdigest()


def cargo_metadata(cargo, manifest, target, graph_flags):
    command = [cargo, "metadata", "--format-version", "1", "--manifest-path", manifest,
               "--filter-platform", target, *graph_flags]
    result = subprocess.run(command, check=True, text=True, capture_output=True, encoding="utf-8")
    return json.loads(result.stdout)


def parse_cargo_args(command, manifest, target):
    graph_flags, index = [], 1
    while index < len(command):
        arg = command[index]
        if arg == "--config":
            if index + 1 == len(command):
                raise ValueError("missing value after --config")
            graph_flags.extend(["--config", command[index + 1]])
            index += 2
        elif arg.startswith("--config="):
            graph_flags.append(arg)
            index += 1
        else:
            break
    if index >= len(command) or command[index] not in ("build", "test"):
        raise ValueError("wrapper supports `cargo build` and `cargo test` only")
    seen_manifest, seen_target, target_count = False, False, 0
    index += 1
    while index < len(command):
        arg = command[index]
        if arg in ("--manifest-path", "--target", "--features", "-F", "--config"):
            if index + 1 == len(command):
                raise ValueError(f"missing value after {arg}")
            value = command[index + 1]
            if arg == "--manifest-path":
                seen_manifest = str(Path(value).resolve()) == str(Path(manifest).resolve())
            elif arg == "--target":
                target_count += 1
                if target_count > 1:
                    raise ValueError("Cargo command contains duplicate --target options")
                seen_target = value == target
            elif arg in ("--features", "-F"):
                graph_flags.extend(["--features", value])
            elif arg == "--config":
                graph_flags.extend(["--config", value])
            index += 2
            continue
        if arg in ("--test", "--example", "--bin", "--bench"):
            if index + 1 == len(command):
                raise ValueError(f"missing value after {arg}")
            index += 2
            continue
        if arg.startswith("--manifest-path="):
            seen_manifest = str(Path(arg.split("=", 1)[1]).resolve()) == str(Path(manifest).resolve())
        elif arg.startswith("--target="):
            target_count += 1
            if target_count > 1:
                raise ValueError("Cargo command contains duplicate --target options")
            seen_target = arg.split("=", 1)[1] == target
        elif arg.startswith("--config="):
            graph_flags.append(arg)
        elif arg.startswith("--features="):
            graph_flags.extend(["--features", arg.split("=", 1)[1]])
        elif arg in ("--all-features", "--no-default-features"):
            graph_flags.append(arg)
        elif arg in ("--locked", "--offline", "--frozen", "--release", "--verbose", "-v", "-vv",
                     "--lib", "--all-targets", "--no-run", "--no-fail-fast", "--doc"):
            if arg in ("--locked", "--offline", "--frozen"):
                graph_flags.append(arg)
        elif arg.startswith("-"):
            raise ValueError(f"unsupported Cargo build flag {arg}; wrapper refuses to guess metadata flags")
        else:
            raise ValueError(f"unsupported Cargo build argument {arg!r}")
        index += 1
    if not seen_manifest or not seen_target:
        raise ValueError("Cargo command must include matching --manifest-path and explicit --target")
    return graph_flags


def fnv(data):
    value = 0xCBF29CE484222325
    for byte in data:
        value = ((value ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{value:016x}"


def read_record(path):
    result = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        key, separator, value = line.partition("=")
        if not separator or not key or key in result:
            raise ValueError(f"invalid or duplicate field in {path}")
        result[key] = value
    if set(result) != {"format", "target", "manifest-hash", "source-hash"}:
        raise ValueError(f"invalid fields in {path}")
    return result


def check_bundle(bundle, target, identity, assume):
    manifest = (bundle / "libdeno-snapshot.manifest").read_bytes()
    if b"format=libdeno-snapshot-2\n" not in manifest:
        raise ValueError("bundle must use snapshot format v2; export a new bundle")
    sidecar_path = bundle / SIDECAR
    if not sidecar_path.exists():
        if not assume:
            raise ValueError("bundle has no source provenance; export with this wrapper or use --assume-compatible only if dependencies are known to match")
        return fnv(manifest)
    sidecar = read_record(sidecar_path)
    if (sidecar.get("format") != "libdeno-snapshot-sources-1"
            or sidecar.get("target") != target
            or sidecar.get("manifest-hash") != fnv(manifest)
            or sidecar.get("source-hash") != identity):
        raise ValueError("bundle source provenance does not match this source tree, target, or manifest; regenerate it for the current dependencies")
    return fnv(manifest)


def write_sidecar(bundle, target, identity, manifest=None):
    manifest_path = bundle / "libdeno-snapshot.manifest"
    manifest = manifest_path.read_bytes() if manifest is None else manifest
    if b"format=libdeno-snapshot-2\n" not in manifest:
        raise ValueError("Cargo export did not produce a snapshot format v2 manifest")
    data = ("format=libdeno-snapshot-sources-1\n"
            f"target={target}\nmanifest-hash={fnv(manifest)}\nsource-hash={identity}\n")
    fd, temp_name = tempfile.mkstemp(prefix=f".{SIDECAR}.", dir=bundle)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as temp:
            temp.write(data)
        Path(temp_name).replace(bundle / SIDECAR)
    finally:
        Path(temp_name).unlink(missing_ok=True)


def publication_lock_path(destination):
    return Path(destination).resolve() / ".libdeno-snapshot.publish-lock"


@contextmanager
def publication_lock(destination):
    destination = Path(destination).resolve()
    # The destination directory itself canonicalizes filesystem case aliases.
    destination.mkdir(parents=True, exist_ok=True)
    lock = publication_lock_path(destination)
    try:
        lock.mkdir()
    except FileExistsError as error:
        raise ValueError(
            f"snapshot export already owns lock {lock}; if no exporter is running, "
            "inspect its owner and remove the stale lock directory manually"
        ) from error
    token = uuid.uuid4().hex
    owner = lock / "owner"
    try:
        owner.write_text(f"pid={os.getpid()}\ntoken={token}\n", encoding="ascii")
    except BaseException:
        try:
            lock.rmdir()
        except OSError:
            pass
        raise
    try:
        yield
    finally:
        try:
            lines = owner.read_text(encoding="ascii").splitlines()
            if len(lines) == 2 and lines[1] == f"token={token}":
                owner.unlink()
                lock.rmdir()
        except FileNotFoundError:
            pass


def publish_bundle(staging, destination, target, identity):
    required = ("libdeno-snapshot.manifest", "CLI_SNAPSHOT.bin", "EXTENSION_RESIDUAL_SOURCES.rs")
    for name in required:
        if not (staging / name).is_file():
            raise ValueError(f"Cargo export did not create {name} in its fresh staging directory")
    if b"format=libdeno-snapshot-2\n" not in (staging / required[0]).read_bytes():
        raise ValueError("Cargo export did not produce a snapshot format v2 manifest")
    manifest = (staging / required[0]).read_bytes()
    files = []
    for path in staging.rglob("*"):
        if path.is_symlink():
            raise ValueError(f"symlink in generated bundle: {path}")
        if path.is_file() and path.name != SIDECAR:
            files.append(path)
    with publication_lock(destination):
        destination.mkdir(parents=True, exist_ok=True)
        sidecar = destination / SIDECAR
        if sidecar.exists() or sidecar.is_symlink():
            sidecar.unlink()
        for source in files:
            target_path = destination / source.relative_to(staging)
            target_path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target_path)
        write_sidecar(destination, target, identity, manifest)


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if "--" not in argv:
        print("snapshot compatibility: provide `-- cargo build ...`", file=sys.stderr)
        return 2
    separator = argv.index("--")
    wrapper_args, command = argv[:separator], argv[separator + 1:]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("import", "export"))
    parser.add_argument("--manifest-path", required=True, type=Path)
    parser.add_argument("--target", required=True)
    parser.add_argument("--bundle", required=True, type=Path)
    parser.add_argument("--assume-compatible", action="store_true")
    args = parser.parse_args(wrapper_args)
    if not command or Path(command[0]).name != "cargo":
        parser.error("provide `-- cargo build ...`")
    if args.assume_compatible and args.operation != "import":
        parser.error("--assume-compatible applies only to imports")
    try:
        manifest = args.manifest_path.resolve()
        bundle = args.bundle.resolve()
        graph_flags = parse_cargo_args(command, str(manifest), args.target)
        identity = graph_identity(cargo_metadata(command[0], str(manifest), args.target, graph_flags), manifest)
        if args.operation == "import":
            manifest_hash = check_bundle(bundle, args.target, identity, args.assume_compatible)
        env = os.environ.copy()
        env.pop("LIBDENO_SNAPSHOT_COMPATIBILITY_FILE", None)
        with tempfile.TemporaryDirectory(prefix="libdeno-snapshot-compat-") as temp:
            staging = Path(temp) / "bundle"
            if args.operation == "export":
                staging.mkdir()
                env.pop("LIBDENO_SNAPSHOT_DIR", None)
                env["LIBDENO_SNAPSHOT_EXPORT_DIR"] = str(staging)
            else:
                env.pop("LIBDENO_SNAPSHOT_EXPORT_DIR", None)
                env["LIBDENO_SNAPSHOT_DIR"] = str(bundle)
            declaration = Path(temp) / "compatibility.txt"
            if args.operation == "import":
                mode = "assumed" if args.assume_compatible else "verified"
                declaration.write_text(
                    "format=libdeno-snapshot-compat-1\n"
                    f"mode={mode}\ntarget={args.target}\nbundle={bundle}\n"
                    f"manifest-hash={manifest_hash}\nsource-hash={identity}\n",
                    encoding="utf-8",
                )
                env["LIBDENO_SNAPSHOT_COMPATIBILITY_FILE"] = str(declaration)
            result = subprocess.run(command, env=env)
            after = graph_identity(cargo_metadata(command[0], str(manifest), args.target, graph_flags), manifest)
            if after != identity:
                raise ValueError("snapshot-relevant dependency sources changed during Cargo; retry with a stable source tree")
            if result.returncode:
                return result.returncode
            if args.operation == "export":
                publish_bundle(staging, bundle, args.target, identity)
        return 0
    except (OSError, ValueError, subprocess.CalledProcessError, KeyError, json.JSONDecodeError) as error:
        print(f"snapshot compatibility: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
