#!/usr/bin/env bats
# Tests for `omemfs pull`

load test_helper/common

setup() {
    setup_test_dir
    setup_local_remote
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# Helper: create two clones where clone2 is behind clone1's push.
setup_two_clones() {
    # clone1 and clone2 both start from empty remote.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    # clone1 pushes a file *after* clone2 has cloned.
    cd clone1
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..
}

@test "pull: already up to date when clone_root matches REMOTE_ROOT" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" myrepo
    [ "$status" -eq 0 ]
    cd myrepo
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [[ "$output" == *"Already up to date"* ]]
}

@test "pull: downloads remote changes to working tree" {
    setup_two_clones
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -f file.txt ]
    [ "$(cat file.txt)" = "hello" ]
}

@test "pull: mtime-only working-tree change writes no new objects on re-pull" {
    # After a pull, an mtime-only change in the working tree must not cause the
    # next pull's working-tree scan to write fresh tree objects (mtime stability
    # via clone_root_entries). Guards against tree-object churn on every pull.
    setup_two_clones
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    before=$(find .omemfs/objects -type f | wc -l)
    # Touch the file's mtime only; content is unchanged.
    touch -d "2020-01-01 00:00:00" file.txt
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    after=$(find .omemfs/objects -type f | wc -l)
    [ "$before" -eq "$after" ]
}

@test "pull: updates clone_root to REMOTE_ROOT after pull" {
    setup_two_clones
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    local clone_root remote_root
    clone_root="$(get_clone_root)"
    remote_root="$(get_remote_root)"
    [ "$clone_root" = "$remote_root" ]
}

@test "pull: aborts on conflict and writes helper files" {
    setup_two_clones

    # clone2: local modification to file.txt
    cd clone2
    echo "local change" > file.txt

    # clone1: push a different change to file.txt
    cd ../clone1
    echo "remote change" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # clone2: pull should exit non-zero and write helper files
    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [[ "$output" == *"conflict"* ]] || [[ "$output" == *"Conflict"* ]]
    # Original file must be unchanged
    [ "$(cat file.txt)" = "local change" ]
    # Helper files must be present
    [ -f file.txt.omemfs-conflict-local ]
    [ -f file.txt.omemfs-conflict-remote ]
    [ "$(cat file.txt.omemfs-conflict-local)" = "local change" ]
    [ "$(cat file.txt.omemfs-conflict-remote)" = "remote change" ]
}

@test "pull: dirty pull (non-overlapping paths) preserves local changes" {
    setup_two_clones

    cd clone2
    echo "new local file" > local_only.txt

    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    # Remote file was applied
    [ -f file.txt ]
    # Local-only file is preserved
    [ -f local_only.txt ]
}

@test "pull: path-scoped pull fetches only the specified subtree" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    # clone1: push files in src/ and docs/.
    cd clone1
    mkdir -p src docs
    echo "source" > src/main.rs
    echo "readme" > docs/README.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2: pull only src/.
    cd clone2
    run "$OMEMFS" pull src
    [ "$status" -eq 0 ]
    # src/main.rs should be present.
    [ -f src/main.rs ]
    [ "$(cat src/main.rs)" = "source" ]
    # docs/ was not part of the scoped pull, so it should not exist yet.
    [ ! -f docs/README.md ]
}

@test "pull: path-scoped pull does not affect clone_root outside the scope" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p src docs
    echo "source" > src/main.rs
    echo "readme" > docs/README.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull src
    [ "$status" -eq 0 ]

    # A subsequent pull of the scoped path should report already up to date.
    run "$OMEMFS" pull src
    [ "$status" -eq 0 ]
    [[ "$output" == *"Already up to date"* ]]
}

@test "pull: scoped pull of an unchanged path downloads no subtree objects" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p src
    echo "source" > src/main.rs
    echo "more"   > src/lib.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    # First scoped pull brings the subtree down.
    run "$OMEMFS" pull src
    [ "$status" -eq 0 ]
    before=$(find .omemfs/objects -type f | wc -l)
    # Second scoped pull: the path is unchanged, so no subtree objects should be
    # fetched (download-before-compare must compare first, fetch only if changed).
    run "$OMEMFS" pull src
    [ "$status" -eq 0 ]
    [[ "$output" == *"Already up to date"* ]]
    after=$(find .omemfs/objects -type f | wc -l)
    [ "$before" -eq "$after" ]
}

@test "pull: --stub-threshold does not download blobs at or above the threshold" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    printf '%050d' 0 > big.txt   # 50 bytes, at/above threshold 10
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    # The path is stubbed, not materialised.
    [ -f big.txt.omemfs-stub ]
    [ ! -f big.txt ]
    # The large blob must NOT have been downloaded into the local cache.
    hash=$(grep -oE '[0-9a-f]{64}' big.txt.omemfs-stub | head -1)
    run object_exists "$hash"
    [ "$status" -ne 0 ]
}

@test "pull: --stub-threshold fetches remote blob on demand to write conflict helper" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    printf 'AAAAAAAAAAAAAAAAAAAA' > big.txt   # 20 bytes remote content
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    # Divergent local content at the same path → add/add conflict.
    printf 'BBBBBBBBBBBBBBBBBBBB' > big.txt
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -ne 0 ]
    # The remote helper must contain the remote content, fetched on demand even
    # though the bulk download skipped this large blob.
    [ -f big.txt.omemfs-conflict-remote ]
    [ "$(cat big.txt.omemfs-conflict-remote)" = "AAAAAAAAAAAAAAAAAAAA" ]
}

@test "pull: --stub-threshold materialises large file inside nested git worktree" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p gitproject
    printf '%050d' 0 > gitproject/data.bin   # large, no .git pushed
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    # A local git worktree exists at gitproject; large files inside it must be
    # materialised (not stubbed), requiring an on-demand blob fetch.
    mkdir -p gitproject
    git -C gitproject init -q
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    [ -f gitproject/data.bin ]
    [ ! -f gitproject/data.bin.omemfs-stub ]
}

@test "pull: conflict writes helper files and exits non-zero" {
    setup_two_clones

    # clone2: pull to sync file.txt (base = "hello")
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    # clone2: local modification
    echo "local change" > file.txt

    # clone1: push a different change to file.txt
    cd ../clone1
    echo "remote change" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # clone2: pull should exit non-zero and write three helper files
    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    # Original file is unchanged
    [ "$(cat file.txt)" = "local change" ]
    # Helper files must exist
    [ -f file.txt.omemfs-conflict-base ]
    [ -f file.txt.omemfs-conflict-local ]
    [ -f file.txt.omemfs-conflict-remote ]
    # base = clone_root content ("hello")
    [ "$(cat file.txt.omemfs-conflict-base)" = "hello" ]
    [ "$(cat file.txt.omemfs-conflict-local)" = "local change" ]
    [ "$(cat file.txt.omemfs-conflict-remote)" = "remote change" ]
}

@test "pull: multi-chunk conflict streams byte-exact helper files for all three sides" {
    # A conflicted multi-chunk file (> 1 MiB CDC_MIN, a few MiB of non-constant
    # data) must produce base/local/remote helper files whose bytes match the
    # original synced content, the local modification, and the remote
    # modification exactly. This exercises the streaming conflict-helper write
    # path (no whole-blob buffering).
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    # Build three distinct ~10 MiB versions (non-constant data so FastCDC splits
    # into multiple chunks; CDC_MIN is 1 MiB).
    head -c $((10 * 1024 * 1024)) /dev/urandom > base.bin
    head -c $((10 * 1024 * 1024)) /dev/urandom > local.bin
    head -c $((10 * 1024 * 1024)) /dev/urandom > remote.bin

    # clone1 pushes the base version after clone2 has cloned.
    cp base.bin clone1/big.bin
    cd clone1
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2 syncs the base version, then modifies it locally. A high
    # --stub-threshold keeps the multi-MiB file materialised (not stubbed), so
    # the working tree holds a real file to conflict on.
    cd clone2
    run "$OMEMFS" pull --stub-threshold 100M
    [ "$status" -eq 0 ]
    cmp ../base.bin big.bin
    cp ../local.bin big.bin
    cd ..

    # clone1 pushes a different modification.
    cp remote.bin clone1/big.bin
    cd clone1
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2 pull -> conflict, three streamed helper files.
    cd clone2
    run "$OMEMFS" pull --stub-threshold 100M
    [ "$status" -ne 0 ]
    [ -f big.bin.omemfs-conflict-base ]
    [ -f big.bin.omemfs-conflict-local ]
    [ -f big.bin.omemfs-conflict-remote ]
    # Byte-exact comparison against the three known versions.
    cmp ../base.bin big.bin.omemfs-conflict-base
    cmp ../local.bin big.bin.omemfs-conflict-local
    cmp ../remote.bin big.bin.omemfs-conflict-remote
    # The working-tree file itself is untouched (still the local modification).
    cmp ../local.bin big.bin
}

@test "pull: text conflict reports conflicting path" {
    setup_two_clones

    cd clone2
    echo "local change" > file.txt

    cd ../clone1
    echo "remote change" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [[ "$output" == *"conflict"* ]] || [[ "$output" == *"Conflict"* ]]
    [[ "$output" == *"file.txt"* ]]
}

@test "pull: text conflict does not update clone_root" {
    setup_two_clones

    # clone1 pushes a file first so clone2 has a clone_root after pulling.
    cd clone1
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    local root_before
    root_before="$(get_clone_root)"

    # clone2: local modification.
    echo "local change" > file.txt

    # clone1: push a different change.
    cd ../clone1
    echo "remote change" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    local root_after
    root_after="$(get_clone_root)"
    [ "$root_before" = "$root_after" ]
}

@test "pull: atomic abort — no change applied when any path conflicts (full pull)" {
    # Atomic-abort policy (design/03, design/04): if ANY path conflicts, NOTHING
    # is applied to the working tree, conflict helpers are written, the command
    # exits non-zero, and clone_root is not updated. Even non-conflicting remote
    # changes are held back until the conflict is resolved.
    setup_two_clones

    # clone1: push file.txt and new.txt
    cd clone1
    echo "hello" > file.txt
    echo "new file content" > new.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2: pull to get both files, then modify only file.txt locally
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "local change" > file.txt
    clone_root_before=$(cat .omemfs/clone_root)

    # clone1: update file.txt and other.txt (new.txt is now remote-only change)
    cd ../clone1
    echo "remote change" > file.txt
    echo "other content" > other.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # clone2: pull — file.txt conflicts, so the entire pull aborts.
    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    # other.txt is a non-conflicting remote change but must NOT be applied.
    [ ! -f other.txt ]
    # file.txt is unchanged (local content preserved)
    [ "$(cat file.txt)" = "local change" ]
    # helper files for the conflict must exist
    [ -f file.txt.omemfs-conflict-local ]
    [ -f file.txt.omemfs-conflict-remote ]
    # clone_root must NOT have been updated.
    [ "$(cat .omemfs/clone_root)" = "$clone_root_before" ]
}

@test "pull: atomic abort — no change applied when any path conflicts (single scoped pull)" {
    setup_two_clones

    cd clone1
    mkdir -p src
    echo "hello" > src/file.txt
    echo "new" > src/new.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "local change" > src/file.txt

    cd ../clone1
    echo "remote change" > src/file.txt
    echo "other content" > src/other.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull src
    [ "$status" -ne 0 ]
    # The non-conflicting remote add (src/other.txt) must NOT be applied.
    [ ! -f src/other.txt ]
    [ "$(cat src/file.txt)" = "local change" ]
    [ -f src/file.txt.omemfs-conflict-local ]
    [ -f src/file.txt.omemfs-conflict-remote ]
}

@test "pull: multi nested scoped pull writes conflict helpers at the correct path" {
    # Regression: in a multi-path scoped pull of nested directories, conflict
    # helper files must be written next to the conflicting file inside the
    # scoped subtree, not at the repository root (scoped-prefix join bug).
    setup_two_clones

    cd clone1
    mkdir -p a/b x/y
    echo "base1" > a/b/f.txt
    echo "base2" > x/y/g.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "local1" > a/b/f.txt

    cd ../clone1
    echo "remote1" > a/b/f.txt
    echo "remote2" > x/y/g.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull a/b x/y
    [ "$status" -ne 0 ]
    # Conflict helpers must be at a/b/f.txt.*, NOT at the repository root.
    [ -f a/b/f.txt.omemfs-conflict-base ]
    [ -f a/b/f.txt.omemfs-conflict-local ]
    [ -f a/b/f.txt.omemfs-conflict-remote ]
    [ ! -f f.txt.omemfs-conflict-base ]
    [ "$(cat a/b/f.txt.omemfs-conflict-local)" = "local1" ]
    [ "$(cat a/b/f.txt.omemfs-conflict-remote)" = "remote1" ]
    # Atomic abort: the non-conflicting x/y/g.txt change must NOT be applied.
    [ "$(cat x/y/g.txt)" = "base2" ]
}

@test "pull: restores mtime from remote tree entry metadata" {
    # clone1 pushes a file with a specific mtime.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    echo "hello" > file.txt
    touch -t 200001010000.00 file.txt
    local original_mtime
    original_mtime="$(stat -c '%Y' file.txt)"
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2 pulls the file; mtime should match the original.
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    local restored_mtime
    restored_mtime="$(stat -c '%Y' file.txt)"
    [ "$restored_mtime" -eq "$original_mtime" ]
}

@test "pull: --stub-threshold creates directory stub for newly added large directory" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    # clone1: push a large directory (40 bytes total, above threshold 10).
    cd clone1
    mkdir -p bigdir
    printf '%020d' 0 > bigdir/a.txt
    printf '%020d' 1 > bigdir/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2: pull with threshold 10; bigdir (40+ bytes) should be directory-stubbed.
    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    # Directory must exist on disk.
    [ -d bigdir ]
    # Individual files must NOT be materialised.
    [ ! -f bigdir/a.txt ]
    [ ! -f bigdir/b.txt ]
    # A directory stub must be inside bigdir.
    [ -f bigdir/.omemfs-stub ]
    # No individual file stubs.
    [ ! -f bigdir/a.txt.omemfs-stub ]
    [ ! -f bigdir/b.txt.omemfs-stub ]
}

@test "pull: --stub-threshold does not directory-stub large dir inside existing local git worktree" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    # clone1: push a large directory inside gitproject (no .git pushed).
    cd clone1
    mkdir -p gitproject/subdir
    printf '%020d' 0 > gitproject/subdir/a.txt
    printf '%020d' 1 > gitproject/subdir/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2: create a local git repo at gitproject before pulling.
    # gitproject/.git exists on disk -> directory stub must not be created inside it.
    cd clone2
    mkdir -p gitproject
    git -C gitproject init -q
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    # Files inside the git worktree must be materialised, not directory-stubbed.
    [ -f gitproject/subdir/a.txt ]
    [ -f gitproject/subdir/b.txt ]
    [ ! -f gitproject/subdir/.omemfs-stub ]
}

@test "pull: remote-deleted conflict does not create omemfs-conflict-remote file" {
    setup_two_clones

    # clone2: pull to get file.txt into clone_root, then locally modify it
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "local change" > file.txt

    # clone1: delete file.txt and push
    cd ../clone1
    rm file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # clone2: pull — file.txt is locally modified but remotely deleted => conflict
    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [[ "$output" == *"conflict"* ]] || [[ "$output" == *"Conflict"* ]]

    # local file must be unchanged
    [ "$(cat file.txt)" = "local change" ]

    # omemfs-conflict-local must exist (local has content)
    [ -f file.txt.omemfs-conflict-local ]

    # omemfs-conflict-remote must NOT exist (remote side deleted the file)
    [ ! -f file.txt.omemfs-conflict-remote ]
}

# ---------------------------------------------------------------------------
# Executable-bit (mode) handling
# ---------------------------------------------------------------------------

@test "pull: remote mode-only change is applied as modified" {
    setup_two_clones

    # clone2: sync file.txt (non-executable) first.
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ ! -x file.txt ]
    cd ..

    # clone1: flip only the executable bit and push.
    cd clone1
    chmod +x file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2: pull must report the path as modified and set the bit.
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [[ "$output" == *"file.txt"* ]]
    [ -x file.txt ]
}

@test "pull: local content change overlapping remote chmod-only change is a conflict" {
    setup_two_clones

    # clone2: sync file.txt first, then modify its content locally.
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "local change" > file.txt
    cd ..

    # clone1: flip only the executable bit and push.
    cd clone1
    chmod +x file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2: pull must abort with a conflict and leave helper files.
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [[ "$output" == *"conflict"* ]] || [[ "$output" == *"Conflict"* ]]
    [ "$(cat file.txt)" = "local change" ]
    [ -f file.txt.omemfs-conflict-local ]
    [ -f file.txt.omemfs-conflict-remote ]
}

@test "pull: modified-on-stub updates stub record without materialising" {
    # When a path is a stub locally and the remote modifies it, pull updates the
    # stub record without downloading the content (design/08 modified-on-stub).
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    # clone1: push a large file (above threshold 10).
    cd clone1
    printf '%020d' 0 > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2: clone with threshold 10 so big.bin is a stub.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 clone2
    [ "$status" -eq 0 ]
    [ -f clone2/big.bin.omemfs-stub ]
    [ ! -f clone2/big.bin ]
    STUB_BEFORE="$(cat clone2/big.bin.omemfs-stub)"

    # clone1: modify big.bin and push.
    cd clone1
    printf '%020d' 9 > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2: pull with threshold 10. The stub record must be updated, and the
    # file must remain unmaterialised (no content download).
    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    [ -f big.bin.omemfs-stub ]
    [ ! -f big.bin ]
    STUB_AFTER="$(cat big.bin.omemfs-stub)"
    # The stub record must have changed (new hash recorded).
    [ "$STUB_BEFORE" != "$STUB_AFTER" ]
    # Expanding must produce the new content.
    run "$OMEMFS" expand big.bin
    [ "$status" -eq 0 ]
    [ "$(cat big.bin)" = "$(printf '%020d' 9)" ]
}

@test "pull: remote deletion of dir-stubbed subtree removes the dir stub" {
    # Regression: a leftover directory stub must not resurrect a deleted subtree.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    # clone1: push a large directory (above threshold 10).
    cd clone1
    mkdir -p bigdir
    printf '%020d' 0 > bigdir/a.txt
    printf '%020d' 1 > bigdir/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2: clone with threshold 10 so bigdir is a directory stub.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 clone2
    [ "$status" -eq 0 ]
    [ -f clone2/bigdir/.omemfs-stub ]

    # clone1: delete the directory and push.
    cd clone1
    rm -rf bigdir
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2: pull. The directory stub must be removed and the directory pruned.
    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    [ ! -f bigdir/.omemfs-stub ]
    [ ! -d bigdir ]

    # A subsequent push must NOT resurrect the deleted subtree on the remote.
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 0 verify
    [ "$status" -eq 0 ]
    [ ! -d verify/bigdir ]
}

# ---------------------------------------------------------------------------
# refactor-instructions.md Phase 8 (E7) step 1: behaviour-pinning tests for
# pull_scoped_multi / pull_scoped, added before the pull/push path
# consolidation and the multi-path scan-scope fix.
# ---------------------------------------------------------------------------

@test "pull: remote-added empty directory is pulled (single path)" {
    setup_two_clones

    cd clone1
    mkdir -p newdir
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull newdir
    [ "$status" -eq 0 ]
    [ -d newdir ]
}

@test "pull: remote-added empty directory is pulled (multi path)" {
    setup_two_clones

    cd clone1
    mkdir -p newdir1 newdir2
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull newdir1 newdir2
    [ "$status" -eq 0 ]
    [ -d newdir1 ]
    [ -d newdir2 ]
}

@test "pull: remote-deleted path removes local files (single path scoped pull)" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p gone
    echo "a" > gone/a.txt
    echo "b" > gone/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull gone
    [ "$status" -eq 0 ]
    [ -f gone/a.txt ]
    cd ..

    cd clone1
    rm -rf gone
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull gone
    [ "$status" -eq 0 ]
    [ ! -d gone ]
}

@test "pull: remote-deleted path removes local files (multi path scoped pull)" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p gone1 gone2
    echo "a" > gone1/a.txt
    echo "b" > gone2/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull gone1 gone2
    [ "$status" -eq 0 ]
    [ -f gone1/a.txt ]
    [ -f gone2/b.txt ]
    cd ..

    cd clone1
    rm -rf gone1 gone2
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull gone1 gone2
    [ "$status" -eq 0 ]
    [ ! -d gone1 ]
    [ ! -d gone2 ]
}

@test "pull: multi-path pull does not scan directories outside the requested paths" {
    # refactor-instructions.md Phase 8 (E7) step 2: pull_scoped used to scan
    # the whole working tree (scan_and_store_with_cache(&work_dir, &work_dir,
    # ...)) even when the caller named specific paths, unlike push_scoped
    # which already scanned per-path. Fixed to scan only each requested path:
    # a multi-path pull must succeed even when an unrelated, out-of-scope
    # directory is unreadable, because that directory must never be scanned.
    # This test was added ahead of the step 2 fix as a target/pinning test
    # (document-first/test-first) and was expected to FAIL until then (F6's
    # "unreadable directory -> error" behaviour turned the old whole-tree
    # scan into a hard error here); it now passes.
    if [ "$(id -u)" -eq 0 ]; then
        skip "test requires a non-root user (root bypasses directory permissions)"
    fi

    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p src docs
    echo "source" > src/main.rs
    echo "readme" > docs/README.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    # An out-of-scope directory, unreadable and not named in the pull command.
    mkdir -p secret
    echo "hidden" > secret/inside.txt
    chmod 000 secret

    run "$OMEMFS" pull src docs
    chmod 700 secret  # restore before any cleanup/rm -rf

    [ "$status" -eq 0 ]
    [ -f src/main.rs ]
    [ -f docs/README.md ]
}

# ---------------------------------------------------------------------------
# [line_merge] auto-merge (design/15_line_history_merge.md)
#
# `omemfs clone --new`'s default .omemfs-filter template already lists
# `.zsh_history` under [line_merge] (src/filter.rs DEFAULT_FILTER_TEMPLATE),
# so these tests use it directly with no manual .omemfs-filter edits.
#
# Note: the regression guard for a plain file NOT matching [line_merge]
# (e.g. file.txt) still conflicting exactly as before is already covered by
# "pull: aborts on conflict and writes helper files" above -- that test
# passes unchanged with the merge pass wired in, so it is not duplicated here.
# ---------------------------------------------------------------------------

@test "pull: line-merges independently appended .zsh_history and does not conflict" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    # clone1: establish a shared base .zsh_history and push it.
    cd clone1
    printf ': 100:0;cmd_100\n' > .zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # clone2: pull the base so both clones share the same clone_root for it,
    # then append a local-only line.
    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ "$(cat .zsh_history)" = ": 100:0;cmd_100" ]
    printf ': 200:0;cmd_local\n' >> .zsh_history

    # clone1: append a remote-only line and push.
    cd ../clone1
    printf ': 300:0;cmd_remote\n' >> .zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # clone2: pull should merge cleanly instead of conflicting.
    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [[ "$output" == *"erged"* ]]
    [ ! -f .zsh_history.omemfs-conflict-base ]
    [ ! -f .zsh_history.omemfs-conflict-local ]
    [ ! -f .zsh_history.omemfs-conflict-remote ]
    grep -q "cmd_100" .zsh_history
    grep -q "cmd_local" .zsh_history
    grep -q "cmd_remote" .zsh_history

    # A subsequent push must succeed (the merged file is dirty relative to
    # the new clone_root, matching the "Dirty pull (no conflict)" semantics).
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
}

@test "pull: line-merges .zsh_history via a scoped pull" {
    # Scoped to a subdirectory (not the bare filename) so this exercises the
    # pd.rel-prefixed diff-map-key join that pull_scoped's per-path loop uses
    # for nested paths (see src/commands/pull.rs pull_scoped, `format!("{}/{}",
    # pd.rel, path)`). A bare leaf-file scope target (e.g. `pull .zsh_history`
    # at the repo root) hits a separate, pre-existing limitation in
    # pull_scoped/diff_trees/splice_into_clone_root (they assume the scoped
    # path resolves to a tree hash) that is unrelated to line_merge and out of
    # scope for this change.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p logs
    printf ': 100:0;cmd_100\n' > logs/.zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull logs
    [ "$status" -eq 0 ]
    [ "$(cat logs/.zsh_history)" = ": 100:0;cmd_100" ]
    printf ': 200:0;cmd_local\n' >> logs/.zsh_history

    cd ../clone1
    printf ': 300:0;cmd_remote\n' >> logs/.zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # clone2: scoped pull (not a full pull) should merge cleanly.
    cd ../clone2
    run "$OMEMFS" pull logs
    [ "$status" -eq 0 ]
    [[ "$output" == *"erged"* ]]
    [ ! -f logs/.zsh_history.omemfs-conflict-base ]
    [ ! -f logs/.zsh_history.omemfs-conflict-local ]
    [ ! -f logs/.zsh_history.omemfs-conflict-remote ]
    grep -q "cmd_100" logs/.zsh_history
    grep -q "cmd_local" logs/.zsh_history
    grep -q "cmd_remote" logs/.zsh_history

    run "$OMEMFS" push
    [ "$status" -eq 0 ]
}

@test "pull: .zsh_history with overlapping same-line edits still conflicts" {
    # An in-place edit of the same base line on both sides (no untouched line
    # to fall back on) cannot be reconciled by the line-level algorithm, so
    # line_merge::merge returns Conflict and the usual helper-file flow runs.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    printf 'orig_line\n' > .zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    printf 'local_edit\n' > .zsh_history

    cd ../clone1
    printf 'remote_edit\n' > .zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [[ "$output" == *"onflict"* ]]
    [ "$(cat .zsh_history)" = "local_edit" ]
    [ -f .zsh_history.omemfs-conflict-base ]
    [ -f .zsh_history.omemfs-conflict-local ]
    [ -f .zsh_history.omemfs-conflict-remote ]
    [ "$(cat .zsh_history.omemfs-conflict-base)" = "orig_line" ]
    [ "$(cat .zsh_history.omemfs-conflict-local)" = "local_edit" ]
    [ "$(cat .zsh_history.omemfs-conflict-remote)" = "remote_edit" ]
}

@test "pull: .zsh_history deleted on one side and modified on the other still conflicts" {
    # The merge pass only applies to an Added/Modified shape on both sides; a
    # delete on one side falls through to the normal conflict flow untouched.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    printf ': 100:0;cmd_100\n' > .zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    rm .zsh_history

    cd ../clone1
    printf ': 200:0;cmd_local\n' >> .zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [[ "$output" == *"onflict"* ]]
    [ ! -f .zsh_history ]
    [ -f .zsh_history.omemfs-conflict-base ]
    [ -f .zsh_history.omemfs-conflict-remote ]
    [ ! -f .zsh_history.omemfs-conflict-local ]
}
