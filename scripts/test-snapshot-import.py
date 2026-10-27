#!/usr/bin/env python3
"""Exercise build.rs snapshot import without compiling or linking Deno/V8."""

import json
import os
import subprocess
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).parents[1]


def fingerprint(data):
    value = 0xCBF29CE484222325
    for byte in data:
        value = ((value ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{value:016x}"


def main():
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
    ledger = manifest["package"]["metadata"]["libdeno"]["upstream"]
    with tempfile.TemporaryDirectory(prefix="libdeno-import-test-") as temp:
        temp = Path(temp)
        binary = temp / "build-script"
        subprocess.run(
            ["rustc", "--edition=2021", "--cfg", 'feature="snapshot-import"',
             "-Awarnings", str(ROOT / "build.rs"), "-o", str(binary)],
            cwd=ROOT, env={**os.environ, "CARGO_PKG_VERSION": manifest["package"]["version"]},
            check=True,
        )
        target_check = temp / "target-check.rs"
        target_check.write_text(
            'mod deno_core { pub mod v8 { pub const VERSION_STRING: &str = "expected"; } }\n'
            f'include!({json.dumps(str(ROOT / "src/snapshot_identity.rs"))});\nfn main() {{}}\n'
        )
        for expected, succeeds in (("expected", True), ("other", False)):
            check = subprocess.run(
                ["rustc", "--edition=2021", str(target_check), "-o", str(temp / "check")],
                env={**os.environ, "LIBDENO_EXPECTED_V8_VERSION": expected},
                text=True, capture_output=True,
            )
            assert (check.returncode == 0) == succeeds, check.stderr
        bundle = temp / "bundle"
        bundle.mkdir()
        snapshot = b"snapshot bytes"
        table = b'("residual", include_str!(concat!(env!("OUT_DIR"), "/residual.js"))),\n'
        residual = b"residual source"
        (bundle / "CLI_SNAPSHOT.bin").write_bytes(snapshot)
        (bundle / "EXTENSION_RESIDUAL_SOURCES.rs").write_bytes(table)
        (bundle / "residual.js").write_bytes(residual)
        identity = {
            "libdeno": manifest["package"]["version"], "target": "test-target",
            "v8": ledger["v8-version"], "typescript": ledger["typescript-version"],
            "deno-core": ledger["deno-core-version"],
            "deno-runtime": ledger["deno-runtime-version"],
        }

        def write_manifest(changes=None, snapshot_bytes=snapshot):
            fields = dict(identity)
            fields.update(changes or {})
            fields.update({"format": "libdeno-snapshot-2",
                           "snapshot-bytes": str(len(snapshot)),
                           "snapshot-hash": fingerprint(snapshot),
                           "table-bytes": str(len(table)), "table-hash": fingerprint(table)})
            (bundle / "libdeno-snapshot.manifest").write_text(
                "".join(f"{key}={value}\n" for key, value in fields.items())
                + f"residual=residual.js\t{len(residual)}\t{fingerprint(residual)}\n")
            if snapshot_bytes != snapshot:
                (bundle / "CLI_SNAPSHOT.bin").write_bytes(snapshot_bytes)

        def run(out, with_bundle=True):
            env = {**os.environ, "OUT_DIR": str(out), "TARGET": "test-target",
                   "CARGO_MANIFEST_DIR": str(ROOT)}
            env.pop("DOCS_RS", None)
            if with_bundle:
                env["LIBDENO_SNAPSHOT_DIR"] = str(bundle)
            else:
                env.pop("LIBDENO_SNAPSHOT_DIR", None)
            return subprocess.run([str(binary)], env=env, text=True, capture_output=True)

        missing = run(temp / "missing", False)
        assert missing.returncode and "requires LIBDENO_SNAPSHOT_DIR" in missing.stderr
        write_manifest()
        out = temp / "valid"
        out.mkdir()
        valid = run(out)
        assert valid.returncode == 0, valid.stderr
        assert f"cargo:rerun-if-changed={bundle}" in valid.stdout
        assert (out / "residual.js").read_bytes() == b"residual source"
        (bundle / "residual.js").write_bytes(b"residual sourcf")
        assert "does not match the manifest" in run(temp / "corrupt-residual").stderr
        (bundle / "residual.js").unlink()
        assert "missing residual source" in run(temp / "missing-residual").stderr
        (bundle / "residual.js").write_bytes(residual)
        write_manifest({"v8": "wrong-v8"})
        assert "v8" in run(temp / "mismatch").stderr
        write_manifest(snapshot_bytes=b"corrupted")
        assert "does not match the manifest" in run(temp / "corrupt").stderr


if __name__ == "__main__":
    main()
