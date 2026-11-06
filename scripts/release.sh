#!/usr/bin/env sh
# Prepare an independent companion release; official ty and Ruff stay pinned.
set -eu
script_root="$(realpath "$(dirname "$0")")"
cd "$(dirname "$script_root")"
if [ "$#" -ne 1 ]; then
    echo "Usage: ./scripts/release.sh VERSION" >&2
    exit 2
fi
python3 scripts/prepare_companion_release.py "$1"
uv lock --no-locked --default-index https://pypi.org/simple
python3 scripts/generate_companion_files.py
