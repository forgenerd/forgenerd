#!/usr/bin/env bash
# Build a static release binary, the same way the release workflow does.
#
#   ./scripts/build.sh                              # x86_64 (default)
#   ./scripts/build.sh aarch64-unknown-linux-musl   # arm64
#
# Needs pnpm, cargo-zigbuild and zig. Works from any directory.
set -euo pipefail

target="${1:-x86_64-unknown-linux-musl}"
cd "$(dirname "$0")/.."

(cd web && pnpm install --frozen-lockfile && pnpm build)   # the UI is embedded into the binary
cargo zigbuild --release --locked --target "$target"

echo "Built target/$target/release/forgenerd"
