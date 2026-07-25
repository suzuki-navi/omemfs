#!/bin/bash
set -euo pipefail

DIST_DIR="${1:-dist}"
mkdir -p "${DIST_DIR}"

echo "===== debug build ====="
cargo build
cp -p target/debug/omemfs "${DIST_DIR}/omemfs-debug"
echo ""

echo "===== release build ====="
cargo build --release
cp -p target/release/omemfs "${DIST_DIR}/omemfs-release"
echo ""

echo "Binaries:"
ls -lh "${DIST_DIR}"/omemfs-*
