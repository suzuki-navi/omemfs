#!/usr/bin/env bats
# Integration tests for the repository lock (.omemfs/clone_root.lock).
#
# The lock is an flock(2) on a persistent lock file (design/12_locking.md).
# These tests hold the lock from an external `flock` process and verify that
# lock-taking omemfs commands fail with the standard lock-held error.

load test_helper/common

setup() {
    setup_repo
    # Skip the whole file if the OS `flock` utility is unavailable: the tests
    # rely on it to hold the lock from an unrelated process.
    if ! command -v flock >/dev/null 2>&1; then
        skip "flock(1) utility not available"
    fi
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# A held lock must block a command that requires it. We assert the command
# exits non-zero and prints the lock-contention message.
assert_blocked() {
    local lock_file=".omemfs/clone_root.lock"
    # Hold the lock in a background flock that sleeps, then try the command.
    flock -x "$lock_file" -c "sleep 5" &
    local holder=$!
    # Give the holder a moment to acquire the lock.
    sleep 0.3
    run "$OMEMFS" "$@"
    kill "$holder" 2>/dev/null || true
    wait "$holder" 2>/dev/null || true
    [ "$status" -ne 0 ]
    [[ "$output" == *"Unable to acquire lock"* ]]
}

@test "lock: held lock blocks push" {
    echo "hello" > file.txt
    assert_blocked push
}

@test "lock: held lock blocks pull" {
    assert_blocked pull
}

@test "lock: held lock blocks pack" {
    echo "hello" > file.txt
    "$OMEMFS" push
    assert_blocked pack
}

@test "lock: held lock blocks expand" {
    assert_blocked expand
}

@test "lock: held lock blocks conflict clean" {
    assert_blocked conflict clean
}

@test "lock: held lock blocks conflict accept-remote" {
    assert_blocked conflict accept-remote
}

@test "lock: lock file persists after a normal command" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # The persistent lock file is created and left in place.
    [ -f ".omemfs/clone_root.lock" ]
}

@test "lock: command succeeds when no lock is held" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
}

# ---------------------------------------------------------------------------
# STAT_CACHE write failure is a non-fatal warning (design/07, item 4).
# ---------------------------------------------------------------------------

@test "stat_cache: unwritable STAT_CACHE does not fail the command" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Make STAT_CACHE unwritable: replace it with a directory so the atomic
    # rename onto the path fails. The command must still succeed and warn.
    rm -f .omemfs/STAT_CACHE
    mkdir .omemfs/STAT_CACHE

    echo "world" > file2.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # The warning (emitted to stderr) is captured in $output by `run`.
    [[ "$output" == *"STAT_CACHE"* ]]
}
