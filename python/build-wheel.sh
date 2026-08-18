#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

if command -v rustup >/dev/null 2>&1 && rustc_path="$(rustup which rustc 2>/dev/null)"; then
    :
else
    rustc_path="$(command -v rustc)"
fi

RUSTC_WRAPPER="" RUSTC="$rustc_path" maturin build --release --locked
