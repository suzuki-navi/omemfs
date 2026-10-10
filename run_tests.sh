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

# Run test files in parallel by default (bats -j) since tests are isolated
# via per-test mktemp dirs. Override with OMEMFS_TEST_JOBS=1 for serial
# execution, or pass -j/--jobs explicitly to take full control.
jobs_opts=()
has_jobs_opt=0
for arg in "$@"; do
    case "$arg" in
        -j|--jobs) has_jobs_opt=1 ;;
    esac
done
if [[ "$has_jobs_opt" -eq 0 ]]; then
    jobs_opts=(-j "${OMEMFS_TEST_JOBS:-$(nproc)}")
fi

exec bats "${jobs_opts[@]}" "$@" "${SCRIPT_DIR}/tests"
