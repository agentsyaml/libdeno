#!/usr/bin/env python3
"""Run cargo-audit with a time-limited exception for the Deno-pinned rustls."""

import argparse
from datetime import date, datetime, timezone
from pathlib import Path
import subprocess
import sys
import tomllib


EXPIRES = date(2026, 11, 17)
RUSTLS_VERSION = "0.23.40"
ADVISORIES = (
    "RUSTSEC-2026-0346",
    "RUSTSEC-2026-0348",
    "RUSTSEC-2026-0349",
)


def audit(lockfile: Path, today: date | None = None) -> int:
    packages = tomllib.loads(lockfile.read_text(encoding="utf-8")).get("package", [])
    pinned_rustls = any(
        package.get("name") == "rustls" and package.get("version") == RUSTLS_VERSION
        for package in packages
    )
    if today is None:
        today = datetime.now(timezone.utc).date()
    if pinned_rustls and today >= EXPIRES:
        print(
            f"Temporary rustls audit exception expired {EXPIRES}; update the Deno stack.",
            file=sys.stderr,
        )
        return 1

    command = ["cargo", "audit", "--deny", "warnings", "--file", str(lockfile)]
    if pinned_rustls:
        print(f"WARNING: temporarily ignoring {', '.join(ADVISORIES)} for rustls {RUSTLS_VERSION} until {EXPIRES}.")
        for advisory in ADVISORIES:
            command.extend(("--ignore", advisory))
    return subprocess.run(command, check=False).returncode


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("lockfile", type=Path)
    return audit(parser.parse_args().lockfile)


if __name__ == "__main__":
    raise SystemExit(main())
