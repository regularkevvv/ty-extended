#!/usr/bin/env sh
#
# Generate ty-extended documentation, toolchain and Python SDK files.
#
# Usage
#
#   ./scripts/autogenerate_files.sh
#
set -eu

script_root="$(realpath "$(dirname "$0")")"
project_root="$(dirname "$script_root")"
cd "$project_root"

echo "Copying the application toolchain..."
cp ./crates/ty_extended/rust-toolchain.toml ./rust-toolchain.toml

echo "Updating the Python lockfile..."
uv lock --no-locked --default-index https://pypi.org/simple

echo "Generating ty-extended documentation and Python SDK..."
python3 scripts/generate_package_files.py
