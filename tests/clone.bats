#!/usr/bin/env bats
# Tests for `omemfs clone`

load test_helper/common

setup() {
    setup_test_dir
    setup_local_remote
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# ---------------------------------------------------------------------------
# Empty remote
# ---------------------------------------------------------------------------

@test "clone: creates .omemfs/ directory" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" myrepo
    [ "$status" -eq 0 ]
    [ -d myrepo/.omemfs ]
}

@test "clone: creates config file" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" myrepo
    [ "$status" -eq 0 ]
    [ -f myrepo/.omemfs/config ]
}

@test "clone: creates clone_root file" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" myrepo
    [ "$status" -eq 0 ]
    [ -f myrepo/.omemfs/clone_root ] || \
        [ "$(cat myrepo/.omemfs/clone_root 2>/dev/null)" = "" ]
}

@test "clone: empty remote succeeds without error" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" myrepo
    [ "$status" -eq 0 ]
}

@test "clone: refuses to clone into non-empty directory without --force" {
    mkdir -p nonempty
    touch nonempty/existing.txt
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" nonempty
    [ "$status" -ne 0 ]
    [[ "$output" == *"not empty"* ]]
}

@test "clone: --force allows cloning into non-empty directory" {
    mkdir -p nonempty
    touch nonempty/existing.txt
    run "$OMEMFS" clone --new --force --url "$REMOTE_DIR" nonempty
    [ "$status" -eq 0 ]
}

@test "clone: --force skips existing paths and leaves their content untouched" {
    # Seed the remote with a file via a first clone+push.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" seed
    [ "$status" -eq 0 ]
    ( cd seed && echo "remote content" > file.txt && "$OMEMFS" push )
    [ "$status" -eq 0 ]

    # Pre-create the destination with a DIFFERENT content at the same path.
    mkdir -p dest
    echo "local content" > dest/file.txt

    run "$OMEMFS" clone --existing --force --url "$REMOTE_DIR" dest
    [ "$status" -eq 0 ]
    # Existing path must be skipped, not overwritten.
    [ "$(cat dest/file.txt)" = "local content" ]
    [[ "$output" == *"existing path(s) skipped"* ]]
}

@test "clone: default remote name is 'origin'" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" myrepo
    [ "$status" -eq 0 ]
    grep -q '"origin"' myrepo/.omemfs/config
}

@test "clone: restores mtime from tree entry metadata" {
    # Push a file with a specific mtime into the remote via clone1.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    cd clone1
    echo "hello" > file.txt
    touch -t 200001010000.00 file.txt
    local original_mtime
    original_mtime="$(stat -c '%Y' file.txt)"
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Clone into clone2 and verify the mtime is restored to the original.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]
    local restored_mtime
    restored_mtime="$(stat -c '%Y' clone2/file.txt)"
    [ "$restored_mtime" -eq "$original_mtime" ]
}

# ---------------------------------------------------------------------------
# Executable-bit (mode) restoration
# ---------------------------------------------------------------------------

@test "clone: executable bit is restored on materialised files" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    echo "#!/bin/sh" > script.sh
    chmod +x script.sh
    echo "plain" > plain.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" dest_repo
    [ "$status" -eq 0 ]
    [ -x dest_repo/script.sh ]
    [ ! -x dest_repo/plain.txt ]
    # The fresh clone must be clean (tree hash includes mode).
    cd dest_repo
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}
