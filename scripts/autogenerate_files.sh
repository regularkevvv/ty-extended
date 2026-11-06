#!/usr/bin/env sh
#
# Generate companion documentation, toolchain and SDK files.
#
# Usage
#
#   ./scripts/autogenerate_files.sh
#
set -eu

script_root="$(realpath "$(dirname "$0")")"
project_root="$(dirname "$script_root")"
cd "$project_root"

echo "Copying the companion toolchain..."
cp ./companion/rust-toolchain.toml ./rust-toolchain.toml

echo "Updating the Python lockfile..."
uv lock --no-locked --default-index https://pypi.org/simple

echo "Generating companion documentation and Python SDK..."
python3 scripts/generate_companion_files.py
