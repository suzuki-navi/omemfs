#!/usr/bin/env bats
# `ls <path>` below a fully stubbed directory (design/04 "Scoped working-tree
# scan"): refused with the same message family as `push <path>`, instead of
# `error: object not found: <hash>` (the stub's tree is not held locally).

load test_helper/common

setup() {
    setup_test_dir
    setup_local_remote
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src
    [ "$status" -eq 0 ]
    cd src
    mkdir -p big/sub
    echo one > big/f1.txt
    echo two > big/f2.txt
    echo deep > big/sub/g.txt
    echo top > top.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 1 dest
    [ "$status" -eq 0 ]
    cd dest
    [ -f big/.omemfs-stub ]
    [ ! -e big/f1.txt ]
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

@test "ls below stub: a file below a fully stubbed directory is refused, not 'object not found'" {
    run "$OMEMFS" ls big/f1.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"an ancestor directory is stubbed"* ]]
    [[ "$output" == *"big/f1.txt"* ]]
    [[ "$output" != *"object not found"* ]]
}

@test "ls below stub: a nested directory and a deeper file are refused too" {
    for p in big/sub big/sub/g.txt big/nope.txt; do
        run "$OMEMFS" ls "$p"
        [ "$status" -ne 0 ]
        [[ "$output" == *"an ancestor directory is stubbed"* ]]
        [[ "$output" != *"object not found"* ]]
    done
}

@test "ls below stub: -r, --dirty and --no-remote are refused the same way" {
    for flags in "-r" "--dirty" "--no-remote" "-r --dirty"; do
        run "$OMEMFS" ls $flags big/f1.txt
        [ "$status" -ne 0 ]
        [[ "$output" == *"an ancestor directory is stubbed"* ]]
    done
}

@test "ls below stub: a multi-path ls names the offending path and lists nothing" {
    run "$OMEMFS" ls top.txt big/f1.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"big/f1.txt"* ]]
    [[ "$output" != *" top.txt"* ]]
}

@test "ls below stub: the stubbed directory itself and the rest of the tree still list" {
    run "$OMEMFS" ls big
    [ "$status" -eq 0 ]
    [[ "$output" == *"big/"* ]]
    run "$OMEMFS" ls top.txt
    [ "$status" -eq 0 ]
    [[ "$output" == *"top.txt"* ]]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
}

@test "ls below stub: once the ancestor is expanded the same path lists normally" {
    run "$OMEMFS" expand big
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls big/f1.txt
    [ "$status" -eq 0 ]
    [[ "$output" == *"big/f1.txt"* ]]
}

@test "ls below stub: agrees with push, which refuses the same path" {
    run "$OMEMFS" push big/f1.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"an ancestor directory is stubbed"* ]]
}

@test "ls below stub: a path under a genuinely deleted (non-stub) directory is still a deletion" {
    cd ../src
    rm -rf big
    run "$OMEMFS" ls big/f1.txt
    [ "$status" -eq 0 ]
    [[ "$output" == *"D"* ]]
    [[ "$output" == *"big/f1.txt"* ]]
}
