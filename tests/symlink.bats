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

# ---------------------------------------------------------------------------
# Write-side symlink escape (design/03_sync_model.md, "Write safety: the
# working-tree write boundary"). The tests above ("Symlink traversal
# safety" elsewhere in the suite, e.g. conflict.bats / restore.bats) already
# cover the read/delete side, which is safe because it uses lstat-based
# `file_type()` and never recurses into a symlinked directory. These tests
# cover the write side: a directory that the *incoming* tree needs to create
# or fill in, but that has been replaced on disk by a symlink pointing
# outside the working tree, must never have content written through it.
# ---------------------------------------------------------------------------

@test "symlink: pull refuses to materialise a blob through a directory symlink" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir sub
    echo "secret content" > sub/child.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    local outside
    outside="$(mktemp -d)"

    # clone2 has never pulled "sub" -- on disk, it is a symlink pointing
    # outside the working tree, standing in for the directory the incoming
    # tree wants to create.
    cd clone2
    ln -s "$outside" sub
    run "$OMEMFS" pull
    cd ..

    # The write must never follow the symlink into $outside: no file leaks
    # out, and the symlink itself is left exactly as it was (not silently
    # replaced by a real directory).
    [ ! -e "$outside/child.txt" ]
    [ -L clone2/sub ]
    [ "$(readlink clone2/sub)" = "$outside" ]

    rm -rf "$outside"
}

@test "symlink: expand refuses to materialise a stubbed directory through a directory symlink" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src
    [ "$status" -eq 0 ]

    cd src
    mkdir sub
    echo "secret content" > sub/child.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # A small stub threshold turns "sub" into a single unexpanded directory
    # stub: dest/sub exists on disk as a real (empty) directory containing
    # only the `.omemfs-stub` marker -- nothing below it is materialised yet.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 1 dest
    [ "$status" -eq 0 ]
    [ -f dest/sub/.omemfs-stub ]
    [ ! -e dest/sub/child.txt ]

    local outside
    outside="$(mktemp -d)"
    # Simulate the directory having been swapped for a symlink after clone:
    # carry the stub marker over to $outside so `expand`'s stub-record read
    # (which follows the symlink) still succeeds and proceeds to try to
    # materialise "child.txt" there.
    cp dest/sub/.omemfs-stub "$outside/.omemfs-stub"
    rm -rf dest/sub
    ln -s "$outside" dest/sub

    cd dest
    run "$OMEMFS" expand sub
    cd ..

    # The write must never follow the symlink into $outside.
    [ ! -e "$outside/child.txt" ]
    [ -L dest/sub ]
    [ "$(readlink dest/sub)" = "$outside" ]

    rm -rf "$outside"
}
