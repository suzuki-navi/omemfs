#!/usr/bin/env bats
# Peak-RSS regression test for the streaming write side.
#
# Pushing a large file must keep peak memory bounded by the streaming design
# budget (~64 MiB) regardless of file size. We push a 128 MiB pseudo-random file
# under /usr/bin/time -v and assert that "Maximum resident set size" stays well
# below 256 MiB. The generous headroom (256 MiB vs the ~64 MiB design budget)
# proves the peak is file-size-independent without being flaky.
#
# Kept in its own file so the slow generation step is easy to identify and skip.

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

@test "peak_rss: push of a 128 MiB file stays under 256 MiB peak RSS" {
    require_slow_tests

    if [ ! -x /usr/bin/time ]; then
        skip "/usr/bin/time not available"
    fi

    # 128 MiB pseudo-random file (non-constant so CDC produces many chunks).
    head -c $((128 * 1024 * 1024)) /dev/urandom > big128.bin

    # Invoke directly (not via bats `run`) so the 2> redirect captures GNU time's
    # report into the file rather than being swallowed by the run wrapper.
    local timefile
    timefile="$(mktemp)"
    local rc=0
    /usr/bin/time -v "$OMEMFS" push 2> "$timefile" || rc=$?
    [ "$rc" -eq 0 ]

    # Parse "Maximum resident set size (kbytes): N".
    local peak_kib
    peak_kib="$(grep -i "Maximum resident set size" "$timefile" | grep -oE '[0-9]+' | tail -1)"
    cat "$timefile" >&2
    rm -f "$timefile"

    [ -n "$peak_kib" ]
    echo "peak RSS: ${peak_kib} KiB"
    [ "$peak_kib" -lt 262144 ]
}

@test "peak_rss: expand of a 128 MiB file stays under 256 MiB peak RSS" {
    require_slow_tests

    if [ ! -x /usr/bin/time ]; then
        skip "/usr/bin/time not available"
    fi

    # Push a 128 MiB pseudo-random file from this working tree.
    head -c $((128 * 1024 * 1024)) /dev/urandom > big128.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Clone into a second working tree with a small stub threshold so the large
    # file is stubbed (not materialised) on clone. Then expand it under GNU time
    # and assert the streaming read keeps peak RSS file-size-independent.
    local clone2
    clone2="$(mktemp -d)"
    "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 1M "$clone2"
    [ -f "$clone2/big128.bin.omemfs-stub" ]

    local timefile
    timefile="$(mktemp)"
    local rc=0
    ( cd "$clone2" && /usr/bin/time -v "$OMEMFS" expand --recursive ) 2> "$timefile" || rc=$?
    [ "$rc" -eq 0 ]
    [ -f "$clone2/big128.bin" ]

    local peak_kib
    peak_kib="$(grep -i "Maximum resident set size" "$timefile" | grep -oE '[0-9]+' | tail -1)"
    cat "$timefile" >&2
    rm -f "$timefile"
    rm -rf "$clone2"

    [ -n "$peak_kib" ]
    echo "peak RSS: ${peak_kib} KiB"
    [ "$peak_kib" -lt 262144 ]
}
