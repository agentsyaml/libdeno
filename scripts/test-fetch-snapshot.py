#!/usr/bin/env python3
"""Exercise snapshot download, verification, extraction, and shell export."""

import hashlib
import os
import subprocess
import tarfile
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "scripts/fetch-snapshot.sh"
VERSION = "1.2.3"
TARGET = "test-target"
ARCHIVE = f"libdeno-{VERSION}-{TARGET}-snapshot.tar.gz"


def main():
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        assets = root / "assets"
        assets.mkdir()
        with tarfile.open(assets / ARCHIVE, "w:gz") as bundle:
            payload = root / "payload.txt"
            payload.write_text("snapshot data\n")
            bundle.add(payload, arcname="payload.txt")
        digest = hashlib.sha256((assets / ARCHIVE).read_bytes()).hexdigest()
        (assets / f"{ARCHIVE}.sha256").write_text(f"{digest}  {ARCHIVE}\n")

        bin_dir = root / "bin"
        bin_dir.mkdir()
        curl = bin_dir / "curl"
        curl.write_text(
            "#!/usr/bin/env python3\n"
            "import os, shutil, sys\n"
            "from pathlib import Path\n"
            "url = next(arg for arg in sys.argv if arg.startswith('https://'))\n"
            "output = Path(sys.argv[sys.argv.index('-o') + 1])\n"
            "shutil.copyfile(Path(os.environ['ASSET_DIR']) / url.rsplit('/', 1)[-1], output)\n"
        )
        curl.chmod(0o755)
        env = os.environ.copy()
        env["PATH"] = f"{bin_dir}{os.pathsep}{env['PATH']}"
        env["ASSET_DIR"] = str(assets)

        work = root / "working dir; $(not-a-command) 'quoted'"
        work.mkdir()
        result = subprocess.run(
            [str(HELPER), VERSION, TARGET], cwd=work, env=env,
            text=True, capture_output=True,
        )
        assert result.returncode == 0, result.stderr
        export = result.stdout.strip().splitlines()[-1]
        actual = subprocess.check_output(
            ["bash", "-c", 'eval "$1"; printf %s "$LIBDENO_SNAPSHOT_DIR"', "test", export],
            text=True,
        )
        expected = str(work.resolve() / f"libdeno-snapshot-{VERSION}-{TARGET}")
        assert actual == expected, (actual, expected)
        assert (Path(actual) / "payload.txt").read_text() == "snapshot data\n"

        (assets / f"{ARCHIVE}.sha256").write_text(f"{'0' * 64}  {ARCHIVE}\n")
        failed_work = root / "checksum failure"
        failed_work.mkdir()
        failed = subprocess.run(
            [str(HELPER), VERSION, TARGET], cwd=failed_work, env=env,
            text=True, capture_output=True,
        )
        assert failed.returncode != 0, "bad checksum unexpectedly succeeded"
        assert "export LIBDENO_SNAPSHOT_DIR=" not in failed.stdout
        assert not (failed_work / f"libdeno-snapshot-{VERSION}-{TARGET}").exists()


if __name__ == "__main__":
    main()
