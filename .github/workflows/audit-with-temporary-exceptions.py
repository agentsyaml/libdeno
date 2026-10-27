#!/usr/bin/env python3
"""Run cargo-audit with time-limited exceptions for Deno-pinned crates."""

import argparse
from dataclasses import dataclass
from datetime import date, datetime, timezone
from pathlib import Path
import subprocess
import sys
import tomllib


@dataclass(frozen=True)
class Exception:
    package: str
    version: str
    advisories: tuple[str, ...]
    expires: date
    reason: str


# Each entry covers a dependency the Deno stack pins. Drop the entry once the
# stack moves to a release that no longer carries the advisory.
EXCEPTIONS = (
    Exception(
        package="rustls",
        version="0.23.40",
        advisories=(
            "RUSTSEC-2026-0346",
            "RUSTSEC-2026-0348",
            "RUSTSEC-2026-0349",
        ),
        expires=date(2026, 11, 17),
        reason="deno_runtime 0.267.0 pins rustls 0.23.40",
    ),
    Exception(
        package="crypto-bigint",
        version="0.5.5",
        advisories=("RUSTSEC-2026-0387",),
        expires=date(2026, 12, 31),
        reason=(
            "elliptic-curve 0.13 in the Deno stack still depends on "
            "crypto-bigint 0.5, while the advisory is fixed in 0.7.6"
        ),
    ),
    Exception(
        package="zstd",
        version="0.13.2",
        advisories=("RUSTSEC-2026-0391",),
        expires=date(2026, 11, 26),
        reason=(
            "deno_node 0.197.0 pins zstd 0.13.2; 0.198.0 and upstream main "
            "still do, and the fixed 0.14.3 chain is not yet available through Deno"
        ),
    ),
    Exception(
        package="zstd-safe",
        version="7.2.4",
        advisories=(
            "RUSTSEC-2026-0403", "RUSTSEC-2026-0398", "RUSTSEC-2026-0395",
            "RUSTSEC-2026-0402", "RUSTSEC-2026-0406", "RUSTSEC-2026-0404",
            "RUSTSEC-2026-0400", "RUSTSEC-2026-0399", "RUSTSEC-2026-0401",
            "RUSTSEC-2026-0405", "RUSTSEC-2026-0397", "RUSTSEC-2026-0396",
        ),
        expires=date(2026, 11, 26),
        reason="Deno's zstd 0.13.2 pin selects zstd-safe 7.2.4; fixed 8.2.0 is not yet available through Deno",
    ),
    Exception(
        package="zstd-sys",
        version="2.0.16+zstd.1.5.7",
        advisories=("RUSTSEC-2026-0410",),
        expires=date(2026, 11, 26),
        reason="Deno's zstd 0.13.2 pin selects zstd-sys 2.0.16; fixed 2.1.2 is not yet available through Deno",
    ),
)


def audit(lockfile: Path, today: date | None = None) -> int:
    packages = tomllib.loads(lockfile.read_text(encoding="utf-8")).get("package", [])
    if today is None:
        today = datetime.now(timezone.utc).date()

    ignored: list[str] = []
    for exception in EXCEPTIONS:
        pinned = any(
            package.get("name") == exception.package
            and package.get("version") == exception.version
            for package in packages
        )
        if not pinned:
            continue
        if today >= exception.expires:
            print(
                f"Temporary audit exception for {exception.package} "
                f"{exception.version} expired {exception.expires}: "
                f"{exception.reason}",
                file=sys.stderr,
            )
            return 1
        print(
            f"WARNING: temporarily ignoring {', '.join(exception.advisories)} for "
            f"{exception.package} {exception.version} until {exception.expires}."
        )
        ignored.extend(exception.advisories)

    command = ["cargo", "audit", "--deny", "warnings", "--file", str(lockfile)]
    for advisory in ignored:
        command.extend(("--ignore", advisory))
    return subprocess.run(command, check=False).returncode


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("lockfile", type=Path)
    return audit(parser.parse_args().lockfile)


if __name__ == "__main__":
    raise SystemExit(main())
