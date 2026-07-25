#!/usr/bin/env bats
# Regression tests for a batch of CLI bug fixes:
#   A: path arguments with a trailing slash (e.g. `expand foo/`) must not be a no-op
#   B: `ls -r` on a clone with stubbed directories must not crash
#   C: `expand` must report the number of files materialised, not stub records
#   D: `conflict accept-remote`/`accept-base` must restore the accepted side's
#      mtime so the resolution is idempotent (no needless re-push)
#   E: `cat` must resolve a short hash prefix against the remote pack index

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
# Helper: build a remote with a large directory subtree, then clone it with a
# small stub threshold so the directory comes down as a stub.
# Leaves $TEST_DIR/dest as the stubbed clone and $TEST_DIR/src as the source.
# ---------------------------------------------------------------------------
seed_stubbed_clone() {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src
    [ "$status" -eq 0 ]
    mkdir -p src/bigdir/sub
    # Several small files plus one large file so the dir aggregate exceeds the
    # threshold and is stubbed as a directory on clone.
    echo "one" > src/bigdir/a.txt
    echo "two" > src/bigdir/b.txt
    echo "three" > src/bigdir/sub/c.txt
    printf '%0200d' 0 > src/bigdir/large.bin
    run bash -c "cd src && '$OMEMFS' push"
    [ "$status" -eq 0 ]

    # Clone with a 50-byte threshold: bigdir (aggregate > 50) is stubbed.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 50 dest
    [ "$status" -eq 0 ]
}

# ---------------------------------------------------------------------------
# B: ls -r must not crash on a stubbed clone
# ---------------------------------------------------------------------------

@test "ls -r: lists a stubbed directory clone without crashing (bug B)" {
    seed_stubbed_clone
    # The directory came down stubbed: only the marker exists, no tree object.
    [ -f dest/bigdir/.omemfs-stub ]

    run bash -c "cd dest && '$OMEMFS' ls -r"
    [ "$status" -eq 0 ]
    # The stubbed directory is shown as a single entry, not descended into.
    [[ "$output" == *"bigdir/"* ]]
}

@test "ls -r: scoped recursive listing of a stub does not crash (bug B)" {
    seed_stubbed_clone
    run bash -c "cd dest && '$OMEMFS' ls -r bigdir/"
    [ "$status" -eq 0 ]
    # The stub is shown as a single self-row, not descended into.
    [[ "$output" == *"bigdir/"* ]]
}

# ---------------------------------------------------------------------------
# A: trailing-slash path arguments must resolve (not silently no-op)
# ---------------------------------------------------------------------------

@test "expand: trailing-slash directory path expands the stub (bug A)" {
    seed_stubbed_clone
    [ -f dest/bigdir/.omemfs-stub ]

    # With a trailing slash, as shell tab-completion appends. Before the fix
    # this printed "Nothing to expand." and changed nothing.
    run bash -c "cd dest && '$OMEMFS' expand -r bigdir/"
    [ "$status" -eq 0 ]
    [[ "$output" != *"Nothing to expand"* ]]
    # The directory stub marker is gone and the files are materialised.
    [ ! -f dest/bigdir/.omemfs-stub ]
    [ -f dest/bigdir/a.txt ]
    [ -f dest/bigdir/sub/c.txt ]
}

@test "expand --dry-run: trailing-slash path is recognised (bug A)" {
    seed_stubbed_clone
    run bash -c "cd dest && '$OMEMFS' expand -r --dry-run bigdir/"
    [ "$status" -eq 0 ]
    [[ "$output" == *"would expand"* ]]
}

# ---------------------------------------------------------------------------
# C: expand reports files materialised, not top-level stub records
# ---------------------------------------------------------------------------

@test "expand: count reflects files materialised, not stub records (bug C)" {
    seed_stubbed_clone
    # bigdir holds 4 files (a.txt, b.txt, sub/c.txt, large.bin). Expanding the
    # single directory stub must report 4 files, not 1.
    run bash -c "cd dest && '$OMEMFS' expand -r bigdir/"
    [ "$status" -eq 0 ]
    [[ "$output" == *"4 file(s) expanded"* ]]
}

# ---------------------------------------------------------------------------
# E: cat resolves a short hash prefix against the remote pack index
# ---------------------------------------------------------------------------

@test "cat: resolves a short hash prefix for a remote-only object (bug E)" {
    seed_stubbed_clone
    # Expand bigdir but keep large.bin stubbed (it is >= the 50-byte threshold),
    # so its blob stays out of the local cache while its 8-char short hash
    # becomes visible in the recursive listing.
    run bash -c "cd dest && '$OMEMFS' expand --stub-threshold 50 bigdir/"
    [ "$status" -eq 0 ]
    [ -f dest/bigdir/large.bin.omemfs-stub ]

    short="$(cd dest && "$OMEMFS" ls -r bigdir 2>/dev/null \
        | grep 'large.bin' | grep -oE '[0-9a-f]{8}' | head -1)"
    [ -n "$short" ]

    # Before the fix this errored with "pack-layer lookup requires a full
    # 64-character hash". Now the prefix resolves against the remote pack index
    # and the blob content is fetched and streamed.
    run bash -c "cd dest && '$OMEMFS' cat '$short'"
    [ "$status" -eq 0 ]
    [[ "$output" != *"requires a full 64-character hash"* ]]
    # The resolved blob is 200 ASCII zeros.
    [[ "$output" == *"00000000"* ]]
}

@test "cat: too-short prefix is still rejected (bug E guard)" {
    seed_stubbed_clone
    run bash -c "cd dest && '$OMEMFS' cat abc"
    [ "$status" -ne 0 ]
    [[ "$output" == *"at least 4 characters"* ]]
}

# ---------------------------------------------------------------------------
# D: accept-remote / accept-base restore mtime → idempotent resolution
# ---------------------------------------------------------------------------

setup_two_clones() {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone_A
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone_B
    [ "$status" -eq 0 ]
}

# Drive A and B to a conflict on shared.txt: both start from a common base,
# then A pushes a remote change and B makes a divergent local change.
drive_to_conflict() {
    setup_two_clones
    # Common baseline pushed from A, pulled into B.
    echo "base" > clone_A/shared.txt
    run bash -c "cd clone_A && '$OMEMFS' push"
    [ "$status" -eq 0 ]
    run bash -c "cd clone_B && '$OMEMFS' pull"
    [ "$status" -eq 0 ]

    # A changes and pushes (remote side).
    echo "remote-wins" > clone_A/shared.txt
    run bash -c "cd clone_A && '$OMEMFS' push"
    [ "$status" -eq 0 ]

    # B changes differently, then pull → conflict.
    echo "local-change" > clone_B/shared.txt
    run bash -c "cd clone_B && '$OMEMFS' pull"
    [ "$status" -ne 0 ]
}

@test "conflict: pull writes a metadata sidecar (bug D)" {
    drive_to_conflict
    [ -f clone_B/shared.txt.omemfs-conflict-meta ]
    [ -f clone_B/shared.txt.omemfs-conflict-remote ]
}

@test "conflict accept-remote: re-push is a no-op after resolution (bug D)" {
    drive_to_conflict

    run bash -c "cd clone_B && '$OMEMFS' conflict accept-remote shared.txt"
    [ "$status" -eq 0 ]
    # Content is the remote version.
    [ "$(cat clone_B/shared.txt)" = "remote-wins" ]
    # All conflict residue removed, including the metadata sidecar.
    [ ! -f clone_B/shared.txt.omemfs-conflict-remote ]
    [ ! -f clone_B/shared.txt.omemfs-conflict-meta ]

    # The remote root before B's push (read from inside a repo dir).
    local root_before
    root_before="$(cd clone_B && "$OMEMFS" cat index-root 2>/dev/null \
        | grep '"remote_root"' | grep -oE '[0-9a-f]{64}')"
    [ -n "$root_before" ]

    # Pushing the resolved tree must NOT change the remote root: the file
    # matches the remote exactly (content + mtime), so the tree hash is equal.
    run bash -c "cd clone_B && '$OMEMFS' push"
    [ "$status" -eq 0 ]

    local root_after
    root_after="$(cd clone_B && "$OMEMFS" cat index-root 2>/dev/null \
        | grep '"remote_root"' | grep -oE '[0-9a-f]{64}')"
    [ "$root_before" = "$root_after" ]
}

@test "conflict accept-remote: resolved file is not dirty against remote (bug D)" {
    drive_to_conflict
    run bash -c "cd clone_B && '$OMEMFS' conflict accept-remote shared.txt"
    [ "$status" -eq 0 ]

    # After accept-remote the working file equals the remote content AND mtime,
    # so a fresh pull reports nothing to apply (already up to date) rather than
    # re-detecting a difference.
    run bash -c "cd clone_B && '$OMEMFS' push && cd ../clone_A && '$OMEMFS' pull"
    [ "$status" -eq 0 ]
}

@test "conflict clean: removes the metadata sidecar too (bug D)" {
    drive_to_conflict
    [ -f clone_B/shared.txt.omemfs-conflict-meta ]
    run bash -c "cd clone_B && '$OMEMFS' conflict clean"
    [ "$status" -eq 0 ]
    [ ! -f clone_B/shared.txt.omemfs-conflict-meta ]
    [ ! -f clone_B/shared.txt.omemfs-conflict-remote ]
}

# ---------------------------------------------------------------------------
# F: stub must not fail with "object not found" when a directory stub already
#    exists in the working tree. Before the fix, stub flattened the entire
#    clone_root through the local-only store up front and aborted as soon as it
#    hit a stubbed subtree whose tree object was absent locally.
# ---------------------------------------------------------------------------

@test "stub: re-stubbing an existing directory stub is a no-op skip (bug F)" {
    seed_stubbed_clone
    # bigdir came down as a directory stub: its tree object is not local.
    [ -f dest/bigdir/.omemfs-stub ]

    run bash -c "cd dest && '$OMEMFS' stub bigdir"
    [ "$status" -eq 0 ]
    [[ "$output" != *"object not found"* ]]
    [[ "$output" == *"already stubbed"* ]]
    # The stub is left in place.
    [ -f dest/bigdir/.omemfs-stub ]
}

@test "stub: a sibling file alongside a directory stub can be stubbed (bug F)" {
    seed_stubbed_clone
    # Add a top-level file, push it, and re-clone with the same threshold so the
    # large bigdir stays a directory stub while top.txt is materialised.
    echo "top-level" > src/top.txt
    run bash -c "cd src && '$OMEMFS' push"
    [ "$status" -eq 0 ]
    rm -rf dest
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 50 dest
    [ "$status" -eq 0 ]
    [ -f dest/bigdir/.omemfs-stub ]
    [ -f dest/top.txt ]

    # Stubbing the materialised sibling must succeed despite the absent bigdir
    # tree object.
    run bash -c "cd dest && '$OMEMFS' stub top.txt"
    [ "$status" -eq 0 ]
    [[ "$output" != *"object not found"* ]]
    [ -f dest/top.txt.omemfs-stub ]
    [ ! -f dest/top.txt ]
}


# ---------------------------------------------------------------------------
# F: invalid explicit scopes must not become an all-stub expansion
# ---------------------------------------------------------------------------

@test "expand: rejects an outside scope without expanding every stub (bug F)" {
    local outside
    seed_stubbed_clone
    outside="$(mktemp -d)"
    [ -f dest/bigdir/.omemfs-stub ]

    run bash -c "cd dest && \"$OMEMFS\" expand -r \"$outside\""
    [ "$status" -ne 0 ]
    [ -f dest/bigdir/.omemfs-stub ]

    rm -rf "$outside"
}

@test "expand: rejects mixed valid and outside scopes atomically (bug F)" {
    local outside
    seed_stubbed_clone
    outside="$(mktemp -d)"
    [ -f dest/bigdir/.omemfs-stub ]

    run bash -c "cd dest && \"$OMEMFS\" expand -r bigdir \"$outside\""
    [ "$status" -ne 0 ]
    [ -f dest/bigdir/.omemfs-stub ]

    rm -rf "$outside"
}


# ---------------------------------------------------------------------------
# G: directory-stub expansion must replace an incorrect existing symlink
# ---------------------------------------------------------------------------

@test "expand: replaces an incorrect symlink in a directory stub (bug G)" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_symlink
    [ "$status" -eq 0 ]
    mkdir -p src_symlink/tree
    echo "payload" > src_symlink/tree/data.txt
    ln -s expected-target src_symlink/tree/link
    run bash -c "cd src_symlink && \"$OMEMFS\" push"
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 1 dest_symlink
    [ "$status" -eq 0 ]
    [ -f dest_symlink/tree/.omemfs-stub ]

    ln -s wrong-target dest_symlink/tree/link
    run bash -c "cd dest_symlink && \"$OMEMFS\" expand -r tree"
    [ "$status" -eq 0 ]
    [ ! -f dest_symlink/tree/.omemfs-stub ]
    [ "$(readlink dest_symlink/tree/link)" = "expected-target" ]
}
