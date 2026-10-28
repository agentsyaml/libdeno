#!/usr/bin/env python3
"""Test source identity and wrapper behavior against patched local Cargo crates."""

import contextlib
import importlib.util
import io
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from unittest import mock
from snapshot_compat_publication_tests import test_case_alias_lock, test_concurrent_publishers
from snapshot_compat_path_tests import test_cross_host_source_order, test_metadata_utf8_decoding

ROOT = Path(__file__).parents[1]
SPEC = importlib.util.spec_from_file_location("snapshot_compat", ROOT / "scripts/snapshot_compat.py")
compat = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compat)


def toml_path(path):
    return json.dumps(Path(path).as_posix())


def package(root, name, version="1.0.0", extra="", package_name=None):
    directory = root / name
    (directory / "src").mkdir(parents=True)
    (directory / "Cargo.toml").write_text(
        f'[package]\nname="{package_name or name}"\nversion="{version}"\nedition="2021"\n{extra}'
    )
    (directory / "src/lib.rs").write_text("pub fn value() {}\n")
    (directory / "source.js").write_text("export const value = 1;\n")
    return directory


def main():
    with tempfile.TemporaryDirectory(prefix="libdeno-compat-test-") as temp:
        root = Path(temp)
        crates = make_fixture(root)
        test_cross_host_source_order(compat, root)
        test_metadata_utf8_decoding(compat)
        consumer = root / "consumer"
        target = host_target()
        metadata = compat.cargo_metadata("cargo", str(consumer / "Cargo.toml"), target, ["--offline"])
        export = compat.graph_identity(metadata, consumer / "Cargo.toml")
        import_metadata = compat.cargo_metadata(
            "cargo", str(consumer / "Cargo.toml"), target,
            ["--offline", "--no-default-features", "--features", "snapshot-import,npm"])
        imported = compat.graph_identity(import_metadata, consumer / "Cargo.toml")
        assert runtime_features(metadata) - runtime_features(import_metadata) == {"snapshot"}
        assert runtime_features(import_metadata) - runtime_features(metadata) == set()
        assert export == imported, "snapshot-creation transport feature must not block reuse"
        test_sources_and_scope(root, crates, consumer, target, export)
        test_wrapper(root, crates, consumer, target, export)
        test_duplicate_targets(root, consumer, target)
        test_config_patch(root, crates, target)
        test_concurrent_publishers(compat, root, target)
        test_case_alias_lock(compat, root)
        test_ambiguous_graph(root, crates, consumer, target)
        test_workspace_guard(root, target)
    print("snapshot compatibility tests passed")


def make_fixture(root):
    v8 = package(root, "v8")
    serde_v8 = package(root, "serde_v8", extra=(
        f"\n[dependencies]\nv8 = {{ version = \"1.0.0\", path = {toml_path(v8)} }}\n"))
    core = package(root, "deno_core", extra=(
        f"\n[dependencies]\nserde_v8 = {{ version = \"1.0.0\", path = {toml_path(serde_v8)} }}\n"))
    web = package(root, "deno_web")
    node = package(root, "deno_node")
    npm_installer = package(root, "deno_npm_installer")
    runtime = package(root, "deno_runtime", extra=(
        f"\n[dependencies]\ndeno_web = {{ version = \"1.0.0\", path = {toml_path(web)} }}\n"
        f"deno_node = {{ version = \"1.0.0\", path = {toml_path(node)} }}\n"
        "\n[features]\nsnapshot = [\"transpile\"]\ntranspile = []\nmeaningful = []\n"))
    libdeno = package(root, "libdeno", extra=(
        "\n[dependencies]\ndeno_core = \"1.0.0\"\n"
        "deno_runtime = { version = \"1.0.0\", features = [\"transpile\"] }\n"
        "deno_node = \"1.0.0\"\n"
        f"deno_npm_installer = {{ version = \"1.0.0\", path = {toml_path(npm_installer)} }}\n\n[features]\n"
        "default = []\nsnapshot = [\"deno_runtime/snapshot\"]\n"
        "snapshot-import = []\nmeaningful = [\"deno_runtime/meaningful\"]\n"))
    unrelated = package(root, "unrelated-core", "9.0.0", package_name="deno_core")
    unrelated_node = package(root, "unrelated-node", "9.0.0", package_name="deno_node")
    unrelated_web = package(root, "unrelated-web", "9.0.0", package_name="deno_web")
    consumer = package(root, "consumer", extra=(
        "\n[dependencies]\nlibdeno = \"1.0.0\"\n"
        f"other_node = {{ package = \"deno_node\", path = {toml_path(unrelated_node)}, version = \"9.0.0\" }}\n"
        f"other_web = {{ package = \"deno_web\", path = {toml_path(unrelated_web)}, version = \"9.0.0\" }}\n\n[features]\n"
        "default = [\"snapshot\", \"npm\"]\nsnapshot = [\"libdeno/snapshot\"]\n"
        "snapshot-import = [\"libdeno/snapshot-import\"]\nnpm = []\n"
        "meaningful = [\"libdeno/meaningful\"]\n\n[dev-dependencies]\n"
        f"other_core = {{ package = \"deno_core\", path = {toml_path(unrelated)}, version = \"9.0.0\" }}\n"
        "\n[patch.crates-io]\n"
        f"libdeno = {{ path = {toml_path(libdeno)} }}\n"
        f"deno_core = {{ path = {toml_path(core)} }}\n"
        f"deno_runtime = {{ path = {toml_path(runtime)} }}\n"
        f"deno_node = {{ path = {toml_path(node)} }}\n"
        f"deno_web = {{ path = {toml_path(web)} }}\n"
        f"serde_v8 = {{ path = {toml_path(serde_v8)} }}\n"
        f"v8 = {{ path = {toml_path(v8)} }}\n"))
    (consumer / "build.rs").write_text(
        'use std::{env, fs, path::Path};\n'
        'fn main() {\n'
        ' println!("cargo:rerun-if-env-changed=LIBDENO_SNAPSHOT_EXPORT_DIR");\n'
        ' if let Ok(dir) = env::var("LIBDENO_SNAPSHOT_EXPORT_DIR") {\n'
        '  let dir = Path::new(&dir); fs::create_dir_all(dir).unwrap();\n'
        '  fs::write(dir.join("libdeno-snapshot.manifest"), b"format=libdeno-snapshot-2\\n").unwrap();\n'
        '  fs::write(dir.join("CLI_SNAPSHOT.bin"), b"fresh snapshot").unwrap();\n'
        '  fs::write(dir.join("EXTENSION_RESIDUAL_SOURCES.rs"), b"table").unwrap();\n'
        '  fs::create_dir_all(dir.join("residual_sources")).unwrap();\n'
        '  fs::write(dir.join("residual_sources/source.js"), b"fresh source").unwrap();\n'
        '  if let Ok(path) = env::var("TEST_STAGE_CAPTURE") { fs::write(path, dir.to_string_lossy().as_bytes()).unwrap(); }\n'
        ' }\n}\n')
    (consumer / "tests").mkdir()
    (consumer / "tests/smoke.rs").write_text("#[test] fn smoke() {}\n")
    return {"deno_core": core, "deno_runtime": runtime, "deno_node": node,
            "deno_web": web, "serde_v8": serde_v8, "v8": v8, "libdeno": libdeno,
            "npm_installer": npm_installer,
            "unrelated": unrelated, "unrelated_node": unrelated_node,
            "unrelated_web": unrelated_web}


def host_target():
    result = subprocess.run(["rustc", "-vV"], check=True, text=True, capture_output=True)
    return next(line.split(": ", 1)[1] for line in result.stdout.splitlines()
                if line.startswith("host:"))


def runtime_features(metadata):
    package_id = next(package["id"] for package in metadata["packages"]
                      if package["name"] == "deno_runtime")
    node = next(node for node in metadata["resolve"]["nodes"] if node["id"] == package_id)
    return set(node["features"])


def test_sources_and_scope(root, crates, consumer, target, initial):
    original = {"name": "deno_core", "version": "1.0.0", "source": None,
                "manifest_path": str(crates["deno_core"] / "Cargo.toml")}
    relocated = root / "relocated-core"
    shutil.copytree(crates["deno_core"], relocated)
    copy = {**original, "manifest_path": str(relocated / "Cargo.toml")}
    assert compat.package_hash(original, []) == compat.package_hash(copy, [])
    registry_source = {**original, "source": "registry+https://example.invalid/index"}
    assert compat.package_hash(registry_source, []) != compat.package_hash(original, [])
    for name in ("deno_core", "deno_runtime", "deno_node", "deno_web", "serde_v8", "v8"):
        source = crates[name] / "src/lib.rs"
        old = source.read_bytes()
        source.write_bytes(old + b"// same-version patch\n")
        assert current_identity(consumer, target) != initial, f"missed Rust patch in {name}"
        source.write_bytes(old)
        js = crates[name] / "source.js"
        old_js = js.read_bytes()
        js.write_bytes(old_js + b"// patched\n")
        assert current_identity(consumer, target) != initial, f"missed JS patch in {name}"
        js.write_bytes(old_js)
    for suffix in (".c", ".h", ".bin"):
        path = crates["deno_core"] / f"native/input{suffix}"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"embedded bytes")
        assert current_identity(consumer, target) != initial, f"missed native input {suffix}"
        path.unlink()
    for relative in ("src/target/mod.rs", "native/target/input.bin", "src/.cargo-embedded"):
        path = crates["deno_core"] / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"source input")
        assert current_identity(consumer, target) != initial, f"missed {relative}"
        path.unlink()
    generated = crates["deno_core"] / "target/generated.rs"
    generated.parent.mkdir()
    generated.write_text("generated")
    assert current_identity(consumer, target) == initial, "root Cargo target output was included"
    generated.unlink()
    generated.parent.rmdir()
    metadata_files = [crates["deno_core"] / name for name in compat.CARGO_METADATA]
    for path in metadata_files:
        path.write_bytes(b"registry metadata")
    assert current_identity(consumer, target) == initial, "Cargo package metadata was included"
    for path in metadata_files:
        path.unlink()
    nested_metadata = crates["deno_core"] / "native/.cargo_vcs_info.json"
    nested_metadata.write_bytes(b"embedded metadata-like input")
    assert current_identity(consumer, target) != initial, "nested metadata-like source was skipped"
    nested_metadata.unlink()
    (crates["unrelated"] / "src/lib.rs").write_text("pub fn unrelated_patch() {}\n")
    (crates["unrelated_node"] / "source.js").write_text("export const unrelated = true;\n")
    (crates["unrelated_web"] / "source.js").write_text("export const unrelated = true;\n")
    (crates["npm_installer"] / "src/lib.rs").write_text("pub fn installer_only() {}\n")
    assert current_identity(consumer, target) == initial, "unrelated consumer/dev package poisoned identity"
    meaningful = compat.cargo_metadata("cargo", str(consumer / "Cargo.toml"), target,
                                       ["--offline", "--features", "meaningful"])
    assert compat.graph_identity(meaningful, consumer / "Cargo.toml") != initial


def current_identity(consumer, target):
    metadata = compat.cargo_metadata("cargo", str(consumer / "Cargo.toml"), target, ["--offline"])
    return compat.graph_identity(metadata, consumer / "Cargo.toml")


def bundle_for(root, target, source_hash):
    bundle = root / "bundle"
    bundle.mkdir(parents=True, exist_ok=True)
    manifest = b"format=libdeno-snapshot-2\n"
    (bundle / "libdeno-snapshot.manifest").write_bytes(manifest)
    (bundle / compat.SIDECAR).write_text(
        "format=libdeno-snapshot-sources-1\n"
        f"target={target}\nmanifest-hash={compat.fnv(manifest)}\nsource-hash={source_hash}\n")
    return bundle


def wrapper_args(consumer, target, bundle, operation="import", extra=()):
    manifest = consumer / "Cargo.toml"
    return [operation, "--manifest-path", str(manifest), "--target", target,
            "--bundle", str(bundle), *extra, "--", "cargo", "test", "--locked",
            "--offline", "--test", "smoke", "--no-default-features",
            "--features", "snapshot-import,npm", "--manifest-path", str(manifest),
            "--target", target]


def export_args(consumer, target, bundle):
    manifest = consumer / "Cargo.toml"
    return ["export", "--manifest-path", str(manifest), "--target", target,
            "--bundle", str(bundle), "--", "cargo", "build", "--locked", "--offline",
            "--lib", "--manifest-path", str(manifest), "--target", target]


def test_config_patch(root, crates, target):
    consumer = package(root, "config-consumer", extra="\n[dependencies]\nlibdeno = \"1.0.0\"\n")
    config = root / "cargo-config.toml"
    config.write_text("[patch.crates-io]\n" + "".join(
        f"{name} = {{ path = {toml_path(crate)} }}\n"
        for name, crate in crates.items()
        if name in {"libdeno", "deno_core", "deno_runtime", "deno_node"}))
    manifest = consumer / "Cargo.toml"
    parser_probe = ["cargo", "--config", str(config), "build", "--features", "snapshot",
                    f"--config={config}", "--manifest-path", str(manifest), "--target", target]
    assert compat.parse_cargo_args(parser_probe, str(manifest), target) == [
        "--config", str(config), "--features", "snapshot", f"--config={config}"]
    flags = ["--offline", "--config", str(config)]
    metadata = compat.cargo_metadata("cargo", str(manifest), target, flags)
    identity = compat.graph_identity(metadata, manifest)
    bundle = bundle_for(root / "config-output", target, identity)
    command = ["import", "--manifest-path", str(manifest), "--target", target,
               "--bundle", str(bundle), "--", "cargo", "--config", str(config), "build",
               "--locked", "--offline", "--manifest-path", str(manifest), "--target", target]
    assert compat.main(command) == 0


def test_wrapper(root, crates, consumer, target, identity):
    bundle = bundle_for(root, target, identity)
    sidecar = bundle / compat.SIDECAR
    sidecar.unlink()
    errors = io.StringIO()
    with contextlib.redirect_stderr(errors):
        result = compat.main(wrapper_args(consumer, target, bundle))
    assert result == 2 and "no source provenance" in errors.getvalue()
    bundle_for(root, target, identity)
    sidecar = bundle / compat.SIDECAR
    sidecar.write_text(sidecar.read_text().replace("sources-1", "sources-0"))
    errors = io.StringIO()
    with contextlib.redirect_stderr(errors):
        result = compat.main(wrapper_args(consumer, target, bundle))
    assert result == 2 and "source provenance" in errors.getvalue()
    bundle_for(root, target, identity)
    assert compat.main(wrapper_args(consumer, target, bundle)) == 0
    sidecar = bundle / compat.SIDECAR
    sidecar.write_text(sidecar.read_text().replace(identity, "0" * 64))
    errors = io.StringIO()
    with contextlib.redirect_stderr(errors):
        result = compat.main(wrapper_args(consumer, target, bundle, extra=("--assume-compatible",)))
    assert result == 2 and "source provenance" in errors.getvalue()
    sidecar.write_text(sidecar.read_text().replace("0" * 64, identity))

    capture = root / "declaration-path"
    real_run = subprocess.run
    source = crates["deno_core"] / "src/lib.rs"
    original_source = source.read_bytes()

    def mutate_after_test(command, *args, **kwargs):
        if command[:2] == ["cargo", "test"]:
            capture.write_text(kwargs["env"]["LIBDENO_SNAPSHOT_COMPATIBILITY_FILE"])
            result = real_run(command, *args, **kwargs)
            source.write_text("pub fn changed_during_cargo() {}\n")
            return result
        return real_run(command, *args, **kwargs)

    errors = io.StringIO()
    with mock.patch.object(compat.subprocess, "run", side_effect=mutate_after_test):
        with contextlib.redirect_stderr(errors):
            result = compat.main(wrapper_args(consumer, target, bundle))
    assert result == 2 and "changed during Cargo" in errors.getvalue()
    assert not Path(capture.read_text()).exists(), "temporary declaration was not cleaned"
    source.write_bytes(original_source)

    export_bundle = root / "failed-export"
    stale = bundle_for(export_bundle, target, identity)
    old_manifest = (stale / "libdeno-snapshot.manifest").read_bytes()
    old_sidecar = (stale / compat.SIDECAR).read_bytes()
    real_run = subprocess.run

    def fail_build(command, *args, **kwargs):
        if command[:2] == ["cargo", "build"]:
            return subprocess.CompletedProcess(command, 1)
        return real_run(command, *args, **kwargs)

    with mock.patch.object(compat.subprocess, "run", side_effect=fail_build):
        result = compat.main(export_args(consumer, target, stale))
    assert result == 1
    assert (stale / "libdeno-snapshot.manifest").read_bytes() == old_manifest
    assert (stale / compat.SIDECAR).read_bytes() == old_sidecar
    errors = io.StringIO()
    with mock.patch.object(compat.subprocess, "run", side_effect=lambda command, *a, **k:
                           subprocess.CompletedProcess(command, 0) if command[:2] == ["cargo", "build"]
                           else real_run(command, *a, **k)):
        with contextlib.redirect_stderr(errors):
            result = compat.main(export_args(consumer, target, stale))
    assert result == 2 and "fresh staging directory" in errors.getvalue()
    assert (stale / "libdeno-snapshot.manifest").read_bytes() == old_manifest
    assert (stale / compat.SIDECAR).read_bytes() == old_sidecar

    (stale / "user-data.txt").write_text("preserve")
    stage_capture = root / "stage-path"
    with mock.patch.dict(os.environ, {"TEST_STAGE_CAPTURE": str(stage_capture)}):
        assert compat.main(export_args(consumer, target, stale)) == 0
    assert not Path(stage_capture.read_text()).exists(), "temporary export stage was not cleaned"
    assert (stale / "CLI_SNAPSHOT.bin").read_bytes() == b"fresh snapshot"
    assert (stale / "residual_sources/source.js").read_bytes() == b"fresh source"
    assert (stale / "user-data.txt").read_text() == "preserve"
    record = compat.read_record(stale / compat.SIDECAR)
    assert record["manifest-hash"] == compat.fnv((stale / "libdeno-snapshot.manifest").read_bytes())
    assert record["source-hash"] == identity


def test_ambiguous_graph(root, crates, consumer, target):
    alternate = root / "alternate-core"
    alternate.mkdir()
    (alternate / "Cargo.toml").write_text(
        '[package]\nname="deno_core"\nversion="2.0.0"\nedition="2021"\n')
    (alternate / "src").mkdir()
    (alternate / "src/lib.rs").write_text("pub fn other() {}\n")
    with (crates["libdeno"] / "Cargo.toml").open("a") as file:
        file.write(f'\n[dependencies.alternate_core]\npackage="deno_core"\nversion="2.0.0"\npath={toml_path(alternate)}\n')
    metadata = compat.cargo_metadata("cargo", str(consumer / "Cargo.toml"), target, ["--offline"])
    try:
        compat.graph_identity(metadata, consumer / "Cargo.toml")
    except ValueError as error:
        assert "deno_core" in str(error) and "found 2" in str(error)
    else:
        raise AssertionError("ambiguous relevant package versions were accepted")


def test_workspace_guard(root, target):
    workspace = root / "workspace"
    member = workspace / "member"
    (workspace / "src").mkdir(parents=True)
    (workspace / "src/lib.rs").write_text("")
    (workspace / "Cargo.toml").write_text(
        '[package]\nname="workspace-root"\nversion="1.0.0"\nedition="2021"\n'
        '[workspace]\nmembers=["member"]\ndefault-members=["member"]\nresolver="2"\n')
    package(workspace, "member")
    metadata = compat.cargo_metadata("cargo", str(workspace / "Cargo.toml"), target, ["--offline"])
    try:
        compat.consumer_package(metadata, workspace / "Cargo.toml")
    except ValueError as error:
        assert "default members differ" in str(error)
    else:
        raise AssertionError("workspace-root default-member mismatch was accepted")
    assert compat.consumer_package(metadata, member / "Cargo.toml")
    real_run = subprocess.run

    def forbid_build(command, *args, **kwargs):
        if command[1] == "build":
            raise AssertionError("workspace guard ran Cargo build before refusing")
        return real_run(command, *args, **kwargs)

    manifest = workspace / "Cargo.toml"
    args = ["export", "--manifest-path", str(manifest), "--target", target,
            "--bundle", str(root / "unused-bundle"), "--", "cargo", "build",
            "--manifest-path", str(manifest), "--target", target]
    errors = io.StringIO()
    with mock.patch.object(compat.subprocess, "run", side_effect=forbid_build):
        with contextlib.redirect_stderr(errors):
            result = compat.main(args)
    assert result == 2 and "default members differ" in errors.getvalue()


def test_duplicate_targets(root, consumer, target):
    manifest = consumer / "Cargo.toml"
    base = ["build", "--manifest-path", str(manifest)]
    for duplicates in (("--target", target, "--target", target),
                       ("--target", target, f"--target={target}"),
                       ("--target", "wrong", f"--target={target}")):
        command = ["cargo", *base, *duplicates]
        try:
            compat.parse_cargo_args(command, str(manifest), target)
        except ValueError as error:
            assert "duplicate --target" in str(error)
        else:
            raise AssertionError(f"duplicate targets accepted: {duplicates}")
    args = ["import", "--manifest-path", str(manifest), "--target", target,
            "--bundle", str(root / "unused"), "--", "cargo", "build",
            "--manifest-path", str(manifest), "--target", target, f"--target={target}"]
    errors = io.StringIO()
    with mock.patch.object(compat.subprocess, "run", side_effect=AssertionError("Cargo was invoked")):
        with contextlib.redirect_stderr(errors):
            result = compat.main(args)
    assert result == 2 and "duplicate --target" in errors.getvalue()


if __name__ == "__main__":
    main()
