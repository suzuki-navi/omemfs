#!/usr/bin/env bats
# Integration tests for two-clone push/pull synchronization with path-scoped push.
#
# Story:
#   1. Clone the same remote into clone_A and clone_B.
#   2. clone_A creates two files and pushes them.
#   3. clone_B pulls and receives both files.
#   4. clone_B updates both files; both appear dirty.
#   5. clone_B pushes only alpha.txt (path-scoped); beta.txt stays dirty.
#   6. clone_A pulls; receives alpha v2 but beta stays at v1.

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
# Helper: set up clone_A and clone_B from the same empty remote.
# After this function both clone_A/ and clone_B/ exist in $TEST_DIR.
# ---------------------------------------------------------------------------
setup_two_clones() {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone_A
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone_B
    [ "$status" -eq 0 ]
}

# ---------------------------------------------------------------------------
# Step 1: both clones start from the same empty root
# ---------------------------------------------------------------------------

@test "two_clone_sync: both clones share the same initial clone_root" {
    setup_two_clones

    # An empty remote may produce no clone_root file; treat missing as empty string.
    local root_a root_b
    root_a="$(cat clone_A/.omemfs/clone_root 2>/dev/null || echo '')"
    root_b="$(cat clone_B/.omemfs/clone_root 2>/dev/null || echo '')"
    [ "$root_a" = "$root_b" ]
}

# ---------------------------------------------------------------------------
# Step 2: clone_A pushes two files
# ---------------------------------------------------------------------------

@test "two_clone_sync: clone_A push succeeds and updates INDEX_ROOT" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    [ -f "$REMOTE_DIR/INDEX_ROOT" ]
}

@test "two_clone_sync: clone_A clone_root matches REMOTE_ROOT after push" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone_root remote_root
    clone_root="$(get_clone_root)"
    remote_root="$(get_remote_root)"
    [ "$clone_root" = "$remote_root" ]
}

@test "two_clone_sync: clone_A is clean after push" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

# ---------------------------------------------------------------------------
# Step 3: clone_B pulls and receives both files
# ---------------------------------------------------------------------------

@test "two_clone_sync: clone_B pull receives both files from clone_A" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ "$(cat alpha.txt)" = "alpha v1" ]
    [ "$(cat beta.txt)"  = "beta v1"  ]
}

@test "two_clone_sync: clone_B clone_root matches REMOTE_ROOT after pull" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]

    local clone_root remote_root
    clone_root="$(get_clone_root)"
    remote_root="$(get_remote_root)"
    [ "$clone_root" = "$remote_root" ]
}

@test "two_clone_sync: clone_B is clean after pull" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

# ---------------------------------------------------------------------------
# Step 4: clone_B updates both files; both appear as dirty
# ---------------------------------------------------------------------------

@test "two_clone_sync: clone_B shows both files dirty after local update" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]

    echo "alpha v2" > alpha.txt
    echo "beta v2"  > beta.txt

    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [[ "$output" == *"alpha.txt"* ]]
    [[ "$output" == *"beta.txt"*  ]]
}

@test "two_clone_sync: clone_root is unchanged before clone_B pushes" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]

    local root_after_pull
    root_after_pull="$(get_clone_root)"

    echo "alpha v2" > alpha.txt
    echo "beta v2"  > beta.txt

    # clone_root must not change merely from working-tree edits.
    [ "$(get_clone_root)" = "$root_after_pull" ]
}

# ---------------------------------------------------------------------------
# Step 5: clone_B pushes only alpha.txt (path-scoped push)
# ---------------------------------------------------------------------------

@test "two_clone_sync: path-scoped push of alpha.txt succeeds" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "alpha v2" > alpha.txt
    echo "beta v2"  > beta.txt

    run "$OMEMFS" push alpha.txt
    [ "$status" -eq 0 ]
}

@test "two_clone_sync: alpha.txt is clean after path-scoped push" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "alpha v2" > alpha.txt
    echo "beta v2"  > beta.txt
    run "$OMEMFS" push alpha.txt
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [[ "$output" != *"alpha.txt"* ]]
}

@test "two_clone_sync: beta.txt remains dirty after path-scoped push of alpha.txt" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "alpha v2" > alpha.txt
    echo "beta v2"  > beta.txt
    run "$OMEMFS" push alpha.txt
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [[ "$output" == *"beta.txt"* ]]
}

@test "two_clone_sync: second path-scoped push of alpha.txt reports nothing to push" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "alpha v2" > alpha.txt
    echo "beta v2"  > beta.txt
    run "$OMEMFS" push alpha.txt
    [ "$status" -eq 0 ]

    run "$OMEMFS" push alpha.txt
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}

# ---------------------------------------------------------------------------
# Step 6: clone_A pulls; receives alpha v2 but beta stays at v1
# ---------------------------------------------------------------------------

@test "two_clone_sync: clone_A pull receives alpha v2 from clone_B" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "alpha v2" > alpha.txt
    echo "beta v2"  > beta.txt
    run "$OMEMFS" push alpha.txt
    [ "$status" -eq 0 ]
    cd ..

    cd clone_A
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ "$(cat alpha.txt)" = "alpha v2" ]
}

@test "two_clone_sync: clone_A pull keeps beta.txt at v1 (not pushed by clone_B)" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "alpha v2" > alpha.txt
    echo "beta v2"  > beta.txt
    run "$OMEMFS" push alpha.txt
    [ "$status" -eq 0 ]
    cd ..

    cd clone_A
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ "$(cat beta.txt)" = "beta v1" ]
}

@test "two_clone_sync: clone_A clone_root matches REMOTE_ROOT after final pull" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    echo "beta v1"  > beta.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "alpha v2" > alpha.txt
    echo "beta v2"  > beta.txt
    run "$OMEMFS" push alpha.txt
    [ "$status" -eq 0 ]
    cd ..

    cd clone_A
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]

    local clone_root remote_root
    clone_root="$(get_clone_root)"
    remote_root="$(get_remote_root)"
    [ "$clone_root" = "$remote_root" ]
}

# ---------------------------------------------------------------------------
# Conflict → resolve → push scenario
# ---------------------------------------------------------------------------

@test "two_clone_sync: conflict blocks push until helper files are removed" {
    setup_two_clones

    # clone_A pushes alpha.txt.
    cd clone_A
    echo "alpha v1" > alpha.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone_B pulls, then makes a local edit.
    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "alpha local" > alpha.txt

    # clone_A pushes a conflicting change.
    cd ../clone_A
    echo "alpha remote" > alpha.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone_B pull produces conflict helper files.
    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [ -f alpha.txt.omemfs-conflict-local ]
    [ -f alpha.txt.omemfs-conflict-remote ]

    # push is blocked while helper files remain.
    echo "alpha resolved" > alpha.txt
    run "$OMEMFS" push
    [ "$status" -ne 0 ]
    [[ "$output" == *"conflict"* ]] || [[ "$output" == *"Conflict"* ]]

    # Remove helper files manually to simulate resolution.
    rm alpha.txt.omemfs-conflict-base alpha.txt.omemfs-conflict-local alpha.txt.omemfs-conflict-remote 2>/dev/null || true

    # Now push should succeed.
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
}

@test "two_clone_sync: restore clears conflict helpers and re-enables push" {
    setup_two_clones

    cd clone_A
    echo "alpha v1" > alpha.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "alpha local" > alpha.txt

    cd ../clone_A
    echo "alpha remote" > alpha.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [ -f alpha.txt.omemfs-conflict-local ]

    # Use restore to accept the remote version (discards local, clears helpers).
    run "$OMEMFS" restore alpha.txt
    [ "$status" -eq 0 ]
    [ ! -f alpha.txt.omemfs-conflict-base ]
    [ ! -f alpha.txt.omemfs-conflict-local ]
    [ ! -f alpha.txt.omemfs-conflict-remote ]

    # pull should now succeed (clone_root was not updated during conflict).
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ "$(cat alpha.txt)" = "alpha remote" ]
}

# ---------------------------------------------------------------------------
# Executable-bit (mode) propagation
# ---------------------------------------------------------------------------

@test "two_clone_sync: executable bit survives push and pull" {
    setup_two_clones

    cd clone_A
    echo "#!/bin/sh" > script.sh
    chmod +x script.sh
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -x script.sh ]
    # B must be fully in sync (tree hash includes mode).
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

@test "two_clone_sync: chmod +x only change propagates via push and pull" {
    setup_two_clones

    # Sync a non-executable file to both clones first.
    cd clone_A
    echo "#!/bin/sh" > script.sh
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ../clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ ! -x script.sh ]
    cd ..

    # A flips only the executable bit and pushes.
    cd clone_A
    chmod +x script.sh
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # B pulls: the bit must be set and B must be clean afterwards.
    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -x script.sh ]
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

@test "two_clone_sync: chmod -x only change propagates via push and pull" {
    setup_two_clones

    # Sync an executable file to both clones first.
    cd clone_A
    echo "#!/bin/sh" > script.sh
    chmod +x script.sh
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ../clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -x script.sh ]
    cd ..

    # A removes the executable bit and pushes.
    cd clone_A
    chmod -x script.sh
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # B pulls: the bit must be cleared and B must be clean afterwards.
    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ ! -x script.sh ]
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

# ---------------------------------------------------------------------------
# CAS: concurrent pushes from two clones — exactly one wins, no silent loss
# ---------------------------------------------------------------------------
#
# Both clones share the same clone_root, then push DIFFERENT changes
# concurrently. With the CAS conditioned on the INDEX_ROOT snapshot read at
# push start, the loser observes that INDEX_ROOT advanced and fails with the
# CAS error; the winner's change is preserved on the remote. Before the fix
# (CAS re-read at finish time), both pushes silently succeed and one change is
# lost.

@test "two_clone_sync: concurrent push — one fails with CAS error, winner preserved" {
    setup_two_clones

    # Both clones share the same (empty) clone_root and push DIFFERENT files at
    # the same time. Each writer captures the INDEX_ROOT snapshot at push start;
    # the read-compare-write at finish is serialized by an flock on the remote.
    # Whichever finishes second sees that INDEX_ROOT advanced and must fail with
    # the CAS error — never silently overwrite the winner.
    echo "from A" > clone_A/a.txt
    echo "from B" > clone_B/b.txt

    local base="$PWD"
    rm -f "$base/a.status" "$base/b.status"
    # Launch both pushes concurrently in fully isolated subshells. Close FD 3
    # (used by bats) and redirect stdin from /dev/null (push would otherwise
    # block on terminal detection), then wait on each by PID.
    bash -c "cd '$base/clone_A' && '$OMEMFS' push > '$base/a.out' 2>&1; echo \$? > '$base/a.status'" 3>&- 0</dev/null &
    local pid_a=$!
    bash -c "cd '$base/clone_B' && '$OMEMFS' push > '$base/b.out' 2>&1; echo \$? > '$base/b.status'" 3>&- 0</dev/null &
    local pid_b=$!
    wait "$pid_a"
    wait "$pid_b"

    [ -f "$base/a.status" ]
    [ -f "$base/b.status" ]

    local sa sb
    sa="$(cat "$base/a.status")"
    sb="$(cat "$base/b.status")"

    # Exactly one must succeed and exactly one must fail (no silent both-success
    # that would lose a change; no spurious both-failure).
    [ "$sa" -ne "$sb" ]

    # The loser must report the CAS error.
    if [ "$sa" -ne 0 ]; then
        grep -q "remote has been updated since last sync" "$base/a.out"
    else
        grep -q "remote has been updated since last sync" "$base/b.out"
    fi

    # The winner's change is on the remote; the loser's is not.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" clone_chk
    [ "$status" -eq 0 ]
    if [ "$sa" -eq 0 ]; then
        [ -f clone_chk/a.txt ]
        [ ! -f clone_chk/b.txt ]
    else
        [ -f clone_chk/b.txt ]
        [ ! -f clone_chk/a.txt ]
    fi
}
