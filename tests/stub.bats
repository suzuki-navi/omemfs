#!/usr/bin/env bats
# Tests for stub / expand functionality

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
# clone --url "$REMOTE_DIR" --stub-threshold
# ---------------------------------------------------------------------------

@test "stub: clone with threshold stubs large files" {
    # Push a small and a large file into the remote.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    echo "tiny" > small.txt
    # Create a file larger than 10 bytes (use 20 bytes).
    printf '%020d' 0 > large.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Clone with a threshold of 10 bytes: large.txt should be stubbed.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]
    # small.txt is below threshold — must be present on disk.
    [ -f dest_repo/small.txt ]
    # large.txt is at/above threshold — must NOT be on disk, but stub must exist.
    [ ! -f dest_repo/large.txt ]
    [ -f dest_repo/large.txt.omemfs-stub ]
}

@test "stub: clone with threshold=0 expands everything" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    printf '%020d' 0 > big.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 0 dest_repo
    [ "$status" -eq 0 ]
    [ -f dest_repo/big.txt ]
    [ ! -f dest_repo/big.txt.omemfs-stub ]
}

# ---------------------------------------------------------------------------
# push with stubs in working tree
# ---------------------------------------------------------------------------

@test "stub: push includes stubbed files in remote tree" {
    # Clone with a threshold so large.txt is stubbed.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    echo "small" > small.txt
    printf '%020d' 0 > large.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]
    # large.txt is stubbed in dest_repo but still present in remote tree.
    # A full push from dest_repo should see nothing to push (stubs == remote).
    cd dest_repo
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}

# ---------------------------------------------------------------------------
# expand
# ---------------------------------------------------------------------------

@test "stub: expand materialises a stubbed file" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    printf '%020d' 0 > large.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]
    [ ! -f dest_repo/large.txt ]

    cd dest_repo
    run "$OMEMFS" expand
    [ "$status" -eq 0 ]
    [ -f large.txt ]
    # Stub must be gone after expansion.
    [ ! -f large.txt.omemfs-stub ]
}

@test "stub: expand --dry-run does not materialise files" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    printf '%020d' 0 > big.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]

    cd dest_repo
    run "$OMEMFS" expand --dry-run
    [ "$status" -eq 0 ]
    [[ "$output" == *"would expand"* ]]
    # File must still be absent.
    [ ! -f big.txt ]
    # Stub must still exist.
    [ -f big.txt.omemfs-stub ]
}

@test "stub: expand --dry-run does not download from the remote" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    printf '%020d' 0 > big.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]

    # Make the remote unreadable: any attempt to fetch a blob must fail. A pure
    # dry-run report must not touch the remote, so it should still succeed.
    rm -rf "$REMOTE_DIR"

    cd dest_repo
    run "$OMEMFS" expand --dry-run
    [ "$status" -eq 0 ]
    [[ "$output" == *"would expand"* ]]
    # The blob must NOT have been written into the local cache by the dry-run.
    hash=$(grep -oE '[0-9a-f]{64}' big.txt.omemfs-stub | head -1)
    run object_exists "$hash"
    [ "$status" -ne 0 ]
}

# ---------------------------------------------------------------------------
# omemfs stub command
# ---------------------------------------------------------------------------

@test "stub: stub command converts a file to a stub" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub file.txt
    [ "$status" -eq 0 ]
    # Real file must be gone.
    [ ! -f file.txt ]
    # Stub must exist.
    [ -f file.txt.omemfs-stub ]
}

@test "stub: stub command removes file and stub is readable by push" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub file.txt
    [ "$status" -eq 0 ]

    # Push from a stubbed state should report nothing to push.
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}

@test "stub: stub command creates directory stub for directory argument" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    mkdir -p sub
    echo "a" > sub/a.txt
    echo "b" > sub/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub sub/
    [ "$status" -eq 0 ]
    # Real files must be gone.
    [ ! -f sub/a.txt ]
    [ ! -f sub/b.txt ]
    # A single directory stub must exist inside the directory.
    [ -f sub/.omemfs-stub ]
    # Individual file stubs must NOT exist.
    [ ! -f sub/a.txt.omemfs-stub ]
    [ ! -f sub/b.txt.omemfs-stub ]
}

@test "stub: directory stub roundtrip with expand restores contents" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    mkdir -p sub
    echo "hello" > sub/a.txt
    echo "world" > sub/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub sub/
    [ "$status" -eq 0 ]
    [ -f sub/.omemfs-stub ]

    run "$OMEMFS" expand sub
    [ "$status" -eq 0 ]
    [ -f sub/a.txt ]
    [ -f sub/b.txt ]
    [ ! -f sub/.omemfs-stub ]
    [ "$(cat sub/a.txt)" = "hello" ]
    [ "$(cat sub/b.txt)" = "world" ]
}

@test "stub: push with directory stub sees no diff" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    mkdir -p sub
    echo "a" > sub/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub sub/
    [ "$status" -eq 0 ]

    # Push from directory-stubbed state should see nothing to push.
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}

@test "stub: scoped push of a file stub keeps remote content (not a deletion)" {
    # Pushing a stubbed file directly must not delete it from the remote.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    echo "keep-me" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    REMOTE_BEFORE="$(get_remote_root)"

    run "$OMEMFS" stub file.txt
    [ "$status" -eq 0 ]
    [ -f file.txt.omemfs-stub ]
    [ ! -f file.txt ]

    # Scoped push of the stubbed path: must be a no-op (content unchanged).
    run "$OMEMFS" push file.txt
    [ "$status" -eq 0 ]
    cd ..

    # The file must still exist on the remote with its original content.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 0 verify
    [ "$status" -eq 0 ]
    [ -f verify/file.txt ]
    [ "$(cat verify/file.txt)" = "keep-me" ]
}

@test "stub: scoped push of a fully-stubbed directory keeps remote content" {
    # Pushing a fully-stubbed directory directly must keep its content on remote.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    mkdir -p sub
    echo "a" > sub/a.txt
    echo "b" > sub/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub sub/
    [ "$status" -eq 0 ]
    [ -f sub/.omemfs-stub ]

    # Scoped push of the stubbed directory: must keep both files on remote.
    run "$OMEMFS" push sub
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 0 verify
    [ "$status" -eq 0 ]
    [ -f verify/sub/a.txt ]
    [ -f verify/sub/b.txt ]
}

@test "stub: partial expansion push merges stub entries with new files" {
    # Stub a directory, then add a new file inside it (partial expansion).
    # A push must merge the stub's recorded tree with the new on-disk file so
    # the remote keeps BOTH the original stubbed files and the new file.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    mkdir -p sub
    echo "original-a" > sub/a.txt
    echo "original-b" > sub/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Stub the directory: only .omemfs-stub remains inside sub/.
    run "$OMEMFS" stub sub/
    [ "$status" -eq 0 ]
    [ -f sub/.omemfs-stub ]
    [ ! -f sub/a.txt ]
    [ ! -f sub/b.txt ]

    # Add a new file alongside the stub marker (partial expansion).
    echo "new-c" > sub/c.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Clone fresh with everything expanded: original files AND the new file
    # must all be present.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 0 verify
    [ "$status" -eq 0 ]
    [ -f verify/sub/a.txt ]
    [ -f verify/sub/b.txt ]
    [ -f verify/sub/c.txt ]
    [ "$(cat verify/sub/a.txt)" = "original-a" ]
    [ "$(cat verify/sub/c.txt)" = "new-c" ]
}

@test "stub: partial expansion new file overrides same-name stub entry" {
    # When a materialised file has the same name as a stub-recorded entry, the
    # materialised file takes priority (design/08 partial expansion merge).
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    mkdir -p sub
    echo "original" > sub/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub sub/
    [ "$status" -eq 0 ]

    # Recreate a.txt with new content while the stub marker is still present.
    echo "overridden" > sub/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 0 verify
    [ "$status" -eq 0 ]
    [ "$(cat verify/sub/a.txt)" = "overridden" ]
}

@test "stub: stub --dry-run does not modify files" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub --dry-run file.txt
    [ "$status" -eq 0 ]
    [[ "$output" == *"would stub"* ]]
    # File must still exist.
    [ -f file.txt ]
    # Stub must not exist.
    [ ! -f file.txt.omemfs-stub ]
}

@test "stub: stub fails when file is not in clone_root" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    echo "untracked" > new.txt

    run "$OMEMFS" stub new.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"clone_root"* ]] || [[ "$output" == *"push"* ]]
}

@test "stub: file with mtime-only change is still rejected when content differs" {
    # After a push, touch the file so mtime is updated but content is different.
    # StatCache will not match due to size/hash mismatch → falls back to full read,
    # which detects the modification and refuses to stub.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    echo "original" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Different content — stat cache miss → full read → mismatch → reject.
    echo "changed" > file.txt
    touch file.txt

    run "$OMEMFS" stub file.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"push"* ]]
}

@test "stub: file matching clone_root with fresh StatCache uses fast path" {
    # After push, StatCache is warm. Stubbing immediately reuses the cached hash
    # without re-reading the file content. The result must be identical to the
    # slow path: stub file created, original removed.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    echo "hello stat cache" > fast.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub fast.txt
    [ "$status" -eq 0 ]
    [ -f fast.txt.omemfs-stub ]
    [ ! -f fast.txt ]
}

@test "stub: stub fails when file has unsaved changes" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    echo "original" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Modify without pushing.
    echo "modified" > file.txt

    run "$OMEMFS" stub file.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"push"* ]]
    # File must still exist unchanged.
    [ -f file.txt ]
}

@test "stub: stub then expand roundtrip preserves content" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    printf '%020d' 42 > data.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub data.bin
    [ "$status" -eq 0 ]
    [ ! -f data.bin ]
    [ -f data.bin.omemfs-stub ]

    run "$OMEMFS" expand data.bin
    [ "$status" -eq 0 ]
    [ -f data.bin ]
    [ ! -f data.bin.omemfs-stub ]
    # Content must match original.
    [[ "$(cat data.bin)" == "$(printf '%020d' 42)" ]]
}

@test "stub: expand --recursive expands all stubs regardless of size" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    printf '%020d' 2 > large.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]
    [ -f dest_repo/large.txt.omemfs-stub ]

    cd dest_repo
    # --recursive must expand even files above default threshold (1M).
    run "$OMEMFS" expand --recursive
    [ "$status" -eq 0 ]
    [ -f large.txt ]
    [ ! -f large.txt.omemfs-stub ]
}

@test "stub: expand --stub-threshold expands only small stubs" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    # small: 5 bytes, large: 20 bytes
    printf '%05d' 1 > small.txt
    printf '%020d' 2 > large.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Clone with threshold 10: large.txt gets stubbed.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]
    [ -f dest_repo/small.txt ]
    [ ! -f dest_repo/large.txt ]
    [ -f dest_repo/large.txt.omemfs-stub ]

    cd dest_repo
    # Stub small.txt to test expand with threshold.
    run "$OMEMFS" stub small.txt
    [ "$status" -eq 0 ]
    [ -f small.txt.omemfs-stub ]
    [ ! -f small.txt ]

    # expand --stub-threshold 10: only small.txt (5B < 10) is expanded;
    # large.txt (20B >= 10) stays stubbed.
    run "$OMEMFS" expand --stub-threshold 10
    [ "$status" -eq 0 ]
    [ -f small.txt ]
    [ ! -f small.txt.omemfs-stub ]
    [ ! -f large.txt ]
    [ -f large.txt.omemfs-stub ]
}

@test "stub: expand with path expands only that directory" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    mkdir -p a b
    printf '%020d' 0 > a/file.txt
    printf '%020d' 1 > b/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Each directory is 20 bytes >= threshold 10, so both are directory-stubbed.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]
    [ -f dest_repo/a/.omemfs-stub ]
    [ -f dest_repo/b/.omemfs-stub ]

    cd dest_repo
    run "$OMEMFS" expand a
    [ "$status" -eq 0 ]
    [ -f a/file.txt ]
    [ ! -f a/.omemfs-stub ]
    # b must remain directory-stubbed.
    [ ! -f b/file.txt ]
    [ -f b/.omemfs-stub ]
}

# ---------------------------------------------------------------------------
# directory stub on clone (tree threshold)
# ---------------------------------------------------------------------------

@test "stub: clone with threshold creates directory stub for large directories" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    mkdir -p bigdir
    # Create enough content so bigdir's total size exceeds 10 bytes.
    printf '%020d' 0 > bigdir/a.txt
    printf '%020d' 1 > bigdir/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Clone with threshold 10: bigdir (40+ bytes total) should be directory-stubbed.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]
    # Individual files must NOT be on disk (directory stub hides them).
    [ ! -f dest_repo/bigdir/a.txt ]
    [ ! -f dest_repo/bigdir/b.txt ]
    # A directory stub must exist inside bigdir.
    [ -f dest_repo/bigdir/.omemfs-stub ]
    # No individual file stubs.
    [ ! -f dest_repo/bigdir/a.txt.omemfs-stub ]
}

@test "stub: clone directory stub can be expanded to restore contents" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    mkdir -p bigdir
    printf '%020d' 0 > bigdir/a.txt
    printf '%020d' 1 > bigdir/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]
    [ -f dest_repo/bigdir/.omemfs-stub ]

    cd dest_repo
    run "$OMEMFS" expand bigdir
    [ "$status" -eq 0 ]
    [ -f bigdir/a.txt ]
    [ -f bigdir/b.txt ]
    [ ! -f bigdir/.omemfs-stub ]
}

# ---------------------------------------------------------------------------
# Git worktree detection: stubs are not created inside nested Git repos
# ---------------------------------------------------------------------------

@test "stub: clone whole-stubs a large git repo root (not partial)" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    mkdir -p gitproject
    git -C gitproject init -q
    # Large file (above threshold 10) inside a git working tree. The git repo
    # root's cumulative size is >= threshold, so the whole repo is stubbed.
    printf '%020d' 0 > gitproject/large.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Clone with threshold 10: gitproject (a git repo root >= threshold) is
    # whole-stubbed as a single directory stub. Nothing inside is materialised.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]
    [ -f dest_repo/gitproject/.omemfs-stub ]
    # The inner content (and .git) must NOT be materialised on disk.
    [ ! -f dest_repo/gitproject/large.txt ]
    [ ! -f dest_repo/gitproject/large.txt.omemfs-stub ]
    [ ! -e dest_repo/gitproject/.git ]

    # expand restores the repo intact, including the file.
    cd dest_repo
    run "$OMEMFS" expand gitproject
    [ "$status" -eq 0 ]
    [ -f gitproject/large.txt ]
    [ ! -f gitproject/.omemfs-stub ]
}

@test "stub: clone whole-stubs a large dir whose subtree is a git repo" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    mkdir -p gitproject/subdir
    git -C gitproject init -q
    # Large directory content inside a git working tree.
    printf '%020d' 0 > gitproject/subdir/a.txt
    printf '%020d' 1 > gitproject/subdir/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Clone with threshold 10: gitproject (a git repo root >= threshold) is
    # whole-stubbed; its subtree is not materialised.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]
    [ -f dest_repo/gitproject/.omemfs-stub ]
    [ ! -e dest_repo/gitproject/subdir ]

    cd dest_repo
    run "$OMEMFS" expand -r gitproject
    [ "$status" -eq 0 ]
    [ -f gitproject/subdir/a.txt ]
    [ -f gitproject/subdir/b.txt ]
}

@test "stub: clone materialises (never partial-stubs) a large file in a materialised git worktree" {
    # The clone root itself is a git worktree (git init at the clone root before
    # push). A large file lives at the root, below no enclosing stubbed dir, so
    # the worktree is descended/materialised — in_git_worktree must prevent the
    # large file from being stubbed.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    git init -q
    # Large file (above threshold 10) at the git worktree root.
    printf '%020d' 0 > large.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Clone with threshold 10: the clone root is a git worktree being
    # materialised, so large.txt must be materialised, not stubbed.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]
    [ -f dest_repo/large.txt ]
    [ ! -f dest_repo/large.txt.omemfs-stub ]
}

# ---------------------------------------------------------------------------
# Lazy clone: stubbed content is not downloaded into the local object cache
# ---------------------------------------------------------------------------

@test "stub: clone does not download objects for a stubbed directory (lazy)" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    mkdir -p bigdir
    # Two files whose combined size puts bigdir above the threshold.
    printf '%020d' 0 > bigdir/a.txt
    printf '%020d' 1 > bigdir/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # Capture the blob hashes so we can assert they are absent from B's cache.
    a_hash=$("$OMEMFS" cat --hash clone-root/bigdir/a.txt)
    b_hash=$("$OMEMFS" cat --hash clone-root/bigdir/b.txt)
    cd ..

    # Clone B with threshold 10: bigdir becomes a directory stub.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]
    [ -f dest_repo/bigdir/.omemfs-stub ]

    # The blob objects for the stubbed directory must NOT have been downloaded
    # into B's local object cache (proving the download was lazy). The local
    # cache shards a hash across directories with adaptive depth, so the full
    # hash is the concatenation of the path components below objects/.
    obj_dir="dest_repo/.omemfs/objects"
    found=0
    if [ -d "$obj_dir" ]; then
        while IFS= read -r f; do
            # Reconstruct the stored hash by stripping the objects/ prefix and
            # removing all path separators.
            rel="${f#"$obj_dir"/}"
            stored="${rel//\//}"
            if [ "$stored" = "$a_hash" ] || [ "$stored" = "$b_hash" ]; then
                found=1
            fi
        done < <(find "$obj_dir" -type f ! -name '.tmp*')
    fi
    [ "$found" -eq 0 ]

    # Now expand and confirm the content materialises correctly.
    cd dest_repo
    run "$OMEMFS" expand -r bigdir
    [ "$status" -eq 0 ]
    [ "$(cat bigdir/a.txt)" = "$(printf '%020d' 0)" ]
    [ "$(cat bigdir/b.txt)" = "$(printf '%020d' 1)" ]
}

# ---------------------------------------------------------------------------
# Executable-bit (mode) restoration on expand
# ---------------------------------------------------------------------------

@test "stub: expand restores executable bit from stub record" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" myrepo
    [ "$status" -eq 0 ]
    cd myrepo
    printf '#!/bin/sh\n%020d' 0 > script.sh
    chmod +x script.sh
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Convert to a stub, then expand it back.
    run "$OMEMFS" stub script.sh
    [ "$status" -eq 0 ]
    [ ! -f script.sh ]
    run "$OMEMFS" expand script.sh
    [ "$status" -eq 0 ]
    [ -f script.sh ]
    [ -x script.sh ]
    # Working tree must match clone_root again.
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

@test "stub: expanding a directory stub restores executable bit of children" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    mkdir bin
    printf '#!/bin/sh\n%020d' 0 > bin/run.sh
    chmod +x bin/run.sh
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Clone with a small threshold so bin/ becomes a directory stub.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 dest_repo
    [ "$status" -eq 0 ]
    [ -f dest_repo/bin/.omemfs-stub ]
    [ ! -f dest_repo/bin/run.sh ]

    # Expand everything: the expand_tree path must restore the bit.
    cd dest_repo
    run "$OMEMFS" expand -r
    [ "$status" -eq 0 ]
    [ -f bin/run.sh ]
    [ -x bin/run.sh ]
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

# ---------------------------------------------------------------------------
# Fix 1 regression: expand must read from remote via pack index (inline entries)
# Fix 2 regression: dir stub is preserved when expand_tree fails mid-way
# ---------------------------------------------------------------------------

@test "stub: expand fetches from pack index when blob is absent from local cache" {
    # Push a small file from source repo (small objects go inline in the delta index).
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    echo "inline-content" > inline.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Clone with stub threshold so the file is stubbed.
    # clone downloads all objects to local cache via transfer_objects.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 1 dest_repo
    [ "$status" -eq 0 ]
    [ -f dest_repo/inline.txt.omemfs-stub ]
    [ ! -f dest_repo/inline.txt ]

    cd dest_repo

    # Delete the blob from the local cache (flat layout at this scale:
    # .omemfs/objects/<full-hash>) to force expand to fetch from the remote pack index.
    local h
    h="$(python -c "import json,sys; d=json.load(open('inline.txt.omemfs-stub')); print(d['hash'])")"
    rm -f ".omemfs/objects/$h"

    # expand must fetch the blob via the remote pack index.
    run "$OMEMFS" expand inline.txt
    [ "$status" -eq 0 ]
    [ -f inline.txt ]
    [ "$(cat inline.txt)" = "inline-content" ]
}

@test "stub: directory stub is preserved when expand_tree fails" {
    # Set up a minimal repository.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" myrepo
    [ "$status" -eq 0 ]
    cd myrepo

    # Create a directory stub pointing to a non-existent tree hash so expand_tree
    # always fails (hash not in local cache or remote).
    mkdir -p mydir
    printf '{"target_type":"tree","hash":"%s","size":100,"mtime":null,"blob_count":1}\n' \
        "dead000000000000000000000000000000000000000000000000000000000000" \
        > mydir/.omemfs-stub

    # expand must fail (tree object does not exist anywhere).
    run "$OMEMFS" expand mydir
    [ "$status" -ne 0 ]

    # Dir stub must still be present — it must not be deleted before expansion succeeds.
    [ -f mydir/.omemfs-stub ]
}

@test "stub: refuses to stub a file whose stub would be visible to Git" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    # Create a nested git worktree with no .gitignore.
    mkdir -p gitproject
    git -C gitproject init -q
    echo "data" > gitproject/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Stubbing must be refused: gitproject/file.txt.omemfs-stub is not gitignored.
    run "$OMEMFS" stub gitproject/file.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"visible to Git"* ]]
    # The file must be untouched.
    [ -f gitproject/file.txt ]
    [ ! -f gitproject/file.txt.omemfs-stub ]
}

@test "stub: allows stubbing inside Git worktree when stub file is gitignored" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    mkdir -p gitproject
    git -C gitproject init -q
    # Ignore all omemfs stub markers in the git worktree.
    echo "*.omemfs-stub" > gitproject/.gitignore
    echo "data" > gitproject/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Stubbing is allowed because the stub file is gitignored.
    run "$OMEMFS" stub gitproject/file.txt
    [ "$status" -eq 0 ]
    [ ! -f gitproject/file.txt ]
    [ -f gitproject/file.txt.omemfs-stub ]
}

@test "stub: chmod on the stub placeholder file is not a content change" {
    # Chmod-ing the .omemfs-stub placeholder itself must not be reported as a
    # modification of the logical entry (design/08): the logical mode comes
    # from the stub record, not the placeholder's filesystem permissions.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub file.txt
    [ "$status" -eq 0 ]
    [ -f file.txt.omemfs-stub ]

    # Flip the executable bit on the placeholder file itself.
    chmod +x file.txt.omemfs-stub

    # The working tree must still match clone_root: nothing to push.
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]

    # ls --dirty must report no changes.
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

# ---------------------------------------------------------------------------
# Lazy pull: a first pull of a small change after a lazy clone must NOT
# download the whole pack set (clone_root tree objects are read lazily).
# ---------------------------------------------------------------------------

@test "stub: first pull of a tiny change after lazy clone does not fetch the whole pack set" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo

    # A large stubbed directory: enough content that the full pack set is
    # comfortably large compared to a single small tree read. The content is
    # INCOMPRESSIBLE (urandom), so the on-the-wire pack bytes really are large —
    # a zero-filled directory would compress to almost nothing and make the
    # read_bytes bound meaningless (the eager pre-download would pass it).
    mkdir -p bigdir
    # 40 * 256 KiB = ~10 MB of unique, incompressible blob data.
    for i in $(seq 1 40); do
        head -c 262144 /dev/urandom > "bigdir/f$i.bin"
    done
    # A small file outside bigdir that we will modify later.
    echo "v1" > small.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Lazy clone B with a small threshold so bigdir becomes a directory stub and
    # its blobs are NOT downloaded into B's cache.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 1024 dest_repo
    [ "$status" -eq 0 ]
    [ -f dest_repo/bigdir/.omemfs-stub ]
    [ ! -f dest_repo/bigdir/f1.bin ]

    # Push a tiny one-file change from A.
    cd src_repo
    echo "v2" > small.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Total size of all pack files in the remote (the eager pre-download used to
    # read essentially all of this on the first pull).
    cd dest_repo
    # Reset the io_stats log so we read only this pull's record.
    rm -f .omemfs/io_stats.jsonl

    run "$OMEMFS" pull
    [ "$status" -eq 0 ]

    # The pulled file is correct.
    [ "$(cat small.txt)" = "v2" ]
    # The stubbed sibling directory remains a stub (not materialised by pull).
    [ -f bigdir/.omemfs-stub ]
    [ ! -f bigdir/f1.bin ]

    # Assert the pull record's read_bytes is well under the total pack size.
    [ -f .omemfs/io_stats.jsonl ]
    local read_bytes
    read_bytes=$(grep '"cmd":"pull"' .omemfs/io_stats.jsonl | tail -1 \
        | grep -oE '"read_bytes":[0-9]+' | grep -oE '[0-9]+$')
    [ -n "$read_bytes" ]

    # The remote holds ~10 MB of incompressible pack content (40 * 256 KiB
    # blobs). A lazy pull of a single ~3-byte change must read FAR less than
    # that. The reads are: the new index root, the changed root tree, and the
    # one changed blob's pack — well under 64 KiB. The previous eager
    # download_missing(remote_root) read essentially the entire pack set (tens
    # of MB), so this tight bound fails if that pre-download is reinstated.
    [ "$read_bytes" -lt 65536 ]
}
