#!/bin/bash
set -e
(cd web && pnpm install && pnpm build)
cargo zigbuild --release --target x86_64-unknown-linux-musl