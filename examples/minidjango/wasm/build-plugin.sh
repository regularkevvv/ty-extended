#!/bin/sh
# Builds the Mini-Django WASM plugin artifact used by this example.
#
# Requires the `wasm32-unknown-unknown` Rust target:
#     rustup target add wasm32-unknown-unknown
set -eu

cd "$(dirname "$0")/plugin"
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/minidjango_plugin.wasm ../.ty/plugins/minidjango.wasm
echo "wrote .ty/plugins/minidjango.wasm"
