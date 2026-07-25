#!/bin/bash
# Run omemfs bats test suite.
# Usage: ./run_tests.sh [bats-options] [test-file ...]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

export OMEMFS="${OMEMFS:-${SCRIPT_DIR}/target/debug/omemfs}"

if [[ ! -x "$OMEMFS" ]]; then
    echo "omemfs binary not found at $OMEMFS"
    echo "Run 'cargo build' first, or set the OMEMFS environment variable."
    exit 1
fi

exec bats "$@" "${SCRIPT_DIR}/tests"
