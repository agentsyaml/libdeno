#!/usr/bin/env bash
# Download, verify, and unpack a prebuilt libdeno snapshot bundle.
#
#   ./scripts/fetch-snapshot.sh <version> [target]
#
# target defaults to the host triple, which is what a native build needs.
# Prints the variable to export on success.
set -euo pipefail

version="${1:-}"
target="${2:-$(rustc -vV | sed -n 's/^host: //p')}"
if [ -z "$version" ] || [ -z "$target" ]; then
  echo "usage: $0 <version, e.g. 0.3.8> [target triple]" >&2
  exit 2
fi

archive="libdeno-$version-$target-snapshot.tar.gz"
base="https://github.com/agentsyaml/libdeno/releases/download/v$version"
dest="libdeno-snapshot-$version-$target"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "downloading $archive"
curl -fsSL "$base/$archive" -o "$tmp/$archive"
curl -fsSL "$base/$archive.sha256" -o "$tmp/$archive.sha256"

(
  cd "$tmp"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$archive.sha256"
  else
    shasum -a 256 -c "$archive.sha256"
  fi
)

mkdir -p "$dest"
tar -xzf "$tmp/$archive" -C "$dest"

echo
printf 'export LIBDENO_SNAPSHOT_DIR=%q\n' "$PWD/$dest"
