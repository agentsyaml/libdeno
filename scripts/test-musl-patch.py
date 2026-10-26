#!/usr/bin/env python3
"""Apply the musl patch to clean pinned crate copies and check its effect."""

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PATCH = ROOT / "patches/deno-musl-0.267.0-0.197.0.patch"


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: test-musl-patch.py DENO_RUNTIME_DIR DENO_NODE_DIR")

    with tempfile.TemporaryDirectory() as temp:
        work = Path(temp)
        for name, source in zip(("deno_runtime", "deno_node"), sys.argv[1:]):
            shutil.copytree(source, work / name)
        subprocess.run(["patch", "-p1", "-i", str(PATCH)], cwd=work, check=True)

        process = (work / "deno_node/ops/process.rs").read_text()
        ctor = process.split("static ARGV_INIT_FN:", 1)[0].rsplit("#[cfg(", 1)[-1]
        assert ctor.startswith('not(target_env = "musl"))]'), "constructor is not excluded on musl"
        assert "#[cfg(target_env = \"musl\")]\n    return;" not in process
        assert "libc::PR_SET_NAME" in process
        host = (work / "deno_runtime/ops/worker_host.rs").read_text()
        worker = (work / "deno_runtime/worker.rs").read_text()
        assert '#[cfg(all(target_os = "linux", not(target_env = "musl")))]' in host
        assert "#[cfg(not(target_env = \"musl\"))]\n        unsafe {\n          libc::malloc_trim" in worker


if __name__ == "__main__":
    main()
