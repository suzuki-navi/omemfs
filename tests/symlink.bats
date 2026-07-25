#!/usr/bin/env bats
# Integration tests for symlink handling across pull and restore.
# Symlink entries are defined in design/01_object_model.md (no hash, no size,
# a `target` string). design/08_stub_system.md states symlinks are always
# materialised (never stubbed).

load test_helper/common

setup() {
    setup_test_dir
    setup_local_remote
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

@test "symlink: pull creates a symlink from the remote tree" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    echo "content" > target.txt
    ln -s target.txt link
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -L link ]
    [ "$(readlink link)" = "target.txt" ]
}

@test "symlink: pull detects a target change as a modification" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    echo "a" > a.txt
    echo "b" > b.txt
    ln -s a.txt link
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2 syncs the initial symlink.
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ "$(readlink link)" = "a.txt" ]
    cd ..

    # clone1 re-points the symlink and pushes.
    cd clone1
    rm link
    ln -s b.txt link
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2 must see the symlink target change as a modification.
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [[ "$output" == *"modified"* ]]
    [ -L link ]
    [ "$(readlink link)" = "b.txt" ]
}

@test "symlink: pull replaces a regular file with a symlink of the same name" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    echo "x" > target.txt
    echo "plain" > item
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -f item ]
    cd ..

    # clone1 replaces the regular file `item` with a symlink.
    cd clone1
    rm item
    ln -s target.txt item
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -L item ]
    [ "$(readlink item)" = "target.txt" ]
}

@test "symlink: pull restores the link's own mtime (lutimes, not the target)" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    echo "content" > target.txt
    ln -s target.txt link
    # Pin the link's own mtime to a fixed past time (-h acts on the link itself).
    touch -h -d "2020-01-02T03:04:05" link
    local src_mtime
    src_mtime=$(stat -c %Y link)
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -L link ]
    local pulled_mtime
    pulled_mtime=$(stat -c %Y link)
    [ "$pulled_mtime" = "$src_mtime" ]
}

@test "symlink: pull then push is idempotent (root does not drift)" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    echo "content" > target.txt
    ln -s target.txt link
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    local root_before
    root_before=$(get_remote_root)
    # A push with no working-tree change must be a no-op: the symlink's mtime
    # must round-trip so the recomputed tree matches the remote root.
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    [[ "$output" == *"Nothing to push"* ]]
    local root_after
    root_after=$(get_remote_root)
    [ "$root_before" = "$root_after" ]
}

@test "symlink: clone restores the link's own mtime" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    echo "content" > target.txt
    ln -s target.txt link
    touch -h -d "2020-01-02T03:04:05" link
    local src_mtime
    src_mtime=$(stat -c %Y link)
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # A fresh clone materialises the symlink via the clone path; its mtime must
    # be restored with lutimes so the recomputed tree still matches the remote.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" clone3
    [ "$status" -eq 0 ]
    cd clone3
    [ -L link ]
    local got_mtime
    got_mtime=$(stat -c %Y link)
    [ "$got_mtime" = "$src_mtime" ]
}

@test "symlink: restore re-creates a symlink deleted from the working tree" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    echo "content" > target.txt
    ln -s target.txt link
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Pin and capture the link's own mtime before deleting it.
    touch -h -d "2020-01-02T03:04:05" link
    local src_mtime
    src_mtime=$(stat -c %Y link)
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Delete the symlink locally, then restore it.
    rm link
    [ ! -e link ]
    run "$OMEMFS" restore link
    [ "$status" -eq 0 ]
    [ -L link ]
    [ "$(readlink link)" = "target.txt" ]
    # The link's own mtime is restored from clone_root.
    [ "$(stat -c %Y link)" = "$src_mtime" ]
}

@test "symlink: restore fixes a symlink whose target was changed locally" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    echo "a" > a.txt
    echo "b" > b.txt
    ln -s a.txt link
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Re-point the symlink locally, then restore it to the clone_root target.
    rm link
    ln -s b.txt link
    run "$OMEMFS" restore link
    [ "$status" -eq 0 ]
    [ -L link ]
    [ "$(readlink link)" = "a.txt" ]
}
