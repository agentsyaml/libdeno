"""Cross-platform path-order regression for snapshot source hashing."""

import inspect
import json
import os
import subprocess
import sys
from pathlib import Path
from pathlib import PurePosixPath, PureWindowsPath
from unittest import mock


class FlavoredPath:
    def __init__(self, value, flavor):
        self.native = Path(os.fspath(value))
        self.flavor = flavor

    @property
    def parent(self):
        return FlavoredPath(self.native.parent, self.flavor)

    def __fspath__(self):
        return os.fspath(self.native)

    def is_symlink(self):
        return self.native.is_symlink()

    def is_dir(self):
        return self.native.is_dir()

    def is_file(self):
        return self.native.is_file()

    def read_bytes(self):
        return self.native.read_bytes()

    def relative_to(self, other):
        relative = self.native.relative_to(other.native).as_posix()
        pure_path = PureWindowsPath if self.flavor == "windows" else PurePosixPath
        return pure_path(relative)

    def __lt__(self, other):
        pure_path = PureWindowsPath if self.flavor == "windows" else PurePosixPath
        return pure_path(self.native.as_posix()) < pure_path(other.native.as_posix())


def test_cross_host_source_order(compat, root):
    package = root / "mixed-case-order"
    for relative, data in (("Cargo.toml", b"manifest"), ("B.rs", b"B"),
                           ("A.rs/nested.c", b"A"), ("a/nested.h", b"a")):
        path = package / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    record = {"name": "fixture", "version": "1.0", "source": None,
              "manifest_path": str(package / "Cargo.toml")}

    def current_hash(flavor):
        namespace = dict(compat.__dict__)
        namespace["Path"] = lambda value: FlavoredPath(value, flavor)
        with mock.patch.object(compat, "Path", namespace["Path"]):
            return compat.package_hash(record, [])

    source = inspect.getsource(compat.package_hash)
    current_order = "sorted(files, key=lambda path: path.relative_to(root).parts)"
    assert current_order in source, "test must track the production ordering expression"
    def legacy_hash(flavor):
        namespace = dict(compat.__dict__)
        namespace["Path"] = lambda value: FlavoredPath(value, flavor)
        exec(compile(source.replace(current_order, "sorted(files)"), "legacy-package-hash", "exec"), namespace)
        return namespace["package_hash"](record, [])

    posix, windows = current_hash("posix"), current_hash("windows")
    old_posix, old_windows = legacy_hash("posix"), legacy_hash("windows")
    assert posix == windows, "production hash depends on host path ordering"
    assert posix == old_posix, "existing POSIX source identities must remain stable"
    assert old_posix != old_windows, "fixture failed to expose the old ordering bug"


def test_metadata_utf8_decoding(compat):
    expected = {"packages": [{"manifest_path": "C:/Users/Jos\u00e9/crate/Cargo.toml",
                              "authors": ["\ud64d\uae38\ub3d9"]}]}
    payload = json.dumps(expected, ensure_ascii=False).encode("utf-8")
    emitter = [sys.executable, "-c", f"import sys; sys.stdout.buffer.write({payload!r})"]
    real_run = subprocess.run

    def emit_metadata(command, **kwargs):
        assert command[:3] == ["cargo", "metadata", "--format-version"]
        return real_run(emitter, **kwargs)

    # Emulate the Windows default without changing the machine's locale.
    with mock.patch.object(subprocess, "_text_encoding", return_value="cp1252"):
        try:
            old = real_run(emitter, check=True, text=True, capture_output=True)
        except UnicodeDecodeError:
            pass
        else:
            assert json.loads(old.stdout) != expected, "fixture did not expose default-decoding corruption"
        with mock.patch.object(compat.subprocess, "run", side_effect=emit_metadata):
            actual = compat.cargo_metadata("cargo", "Cargo.toml", "test-target", [])
        assert actual == expected, "Cargo UTF-8 paths or authors were corrupted"
    print("metadata UTF-8 decoding regression passed (cp1252 default emulated)")
