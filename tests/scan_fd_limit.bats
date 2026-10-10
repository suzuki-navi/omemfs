#!/usr/bin/env bats
# The working-tree scan keeps its directory fds within a fixed budget, so ls,
# push and pull run under a small `ulimit -n` however wide the tree is
# (design/03_sync_model.md, "File descriptor budget"; design/04_cli_spec.md,
# "File descriptor usage").

load test_helper/common

setup() {
    setup_repo
    OTHER=""
}

teardown() {
    [ -n "$OTHER" ] && rm -rf "$OTHER"
    teardown_local_remote
    teardown_test_dir
}

# 400 sibling directories (one file each) and a second nesting level under a few.
make_wide_tree() {
    local i
    for i in $(seq 1 400); do
        mkdir "d$i"
        echo "content $i" > "d$i/f.txt"
    done
    for i in 1 2 3 4 5; do
        mkdir -p "d$i/sub$i/deeper"
        echo "nested $i" > "d$i/sub$i/deeper/g.txt"
    done
}

# Run a command with the soft fd limit lowered in a child shell only, so the
# limit never leaks into the test harness. Sets $status/$output like `run`.
run_limited() {
    local limit="$1"
    shift
    run bash -c 'ulimit -n "$1" || exit 99; shift; exec "$@"' _ "$limit" "$@"
}

@test "scan_fd_limit: push of 400 sibling directories succeeds under ulimit -n 256" {
    make_wide_tree
    run_limited 256 "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

@test "scan_fd_limit: ls and ls --dirty succeed under ulimit -n 256" {
    make_wide_tree
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run_limited 256 "$OMEMFS" ls
    [ "$status" -eq 0 ]
    [[ "$output" == *"d400"* ]]

    echo "edited" > d7/f.txt
    run_limited 256 "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [[ "$output" == *"d7/f.txt"* ]]
}

@test "scan_fd_limit: pull succeeds under ulimit -n 256 after another clone pushed a change" {
    make_wide_tree
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    OTHER="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$OTHER"
    [ "$status" -eq 0 ]

    echo "changed upstream" > d300/f.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd "$OTHER"
    run_limited 256 "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ "$(cat d300/f.txt)" = "changed upstream" ]
}

@test "scan_fd_limit: exhaustion during the scan names the path and the soft limit" {
    local path="" i
    for i in $(seq 1 40); do
        path="$path/l$i"
    done
    mkdir -p ".$path"
    echo deep > ".$path/leaf.txt"

    run_limited 16 "$OMEMFS" ls
    if [[ "$output" != *"too many open files"* ]]; then
        skip "the limit did not exhaust descriptors on this host: $output"
    fi
    [ "$status" -eq 1 ]
    [[ "$output" == *"too many open files while scanning '"* ]]
    [[ "$output" == *"RLIMIT_NOFILE soft limit: 16"* ]]
    [[ "$output" == *"raise it with \`ulimit -n\`"* ]]
}
