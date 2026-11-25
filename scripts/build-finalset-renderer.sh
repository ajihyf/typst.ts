#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RENDERER_DIR="${ROOT_DIR}/packages/renderer"

cd "${RENDERER_DIR}"

wasm-pack build \
  --target web \
  --scope myriaddreamin \
  -- \
  --no-default-features \
  --features web

node ../tools/wasm-debundle.mjs

TARBALL="$(npm pack --silent)"
echo "Built renderer tarball: ${RENDERER_DIR}/${TARBALL}"
