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

@test "pull: path-scoped pull updates .omemfs-filter as a regular file" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    cd clone1
    printf '[ignore]\nold/\n' > .omemfs-filter
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    printf '[ignore]\nnew/\n' > .omemfs-filter
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ../clone2

    run "$OMEMFS" pull .omemfs-filter
    [ "$status" -eq 0 ]
    [ "$(cat .omemfs-filter)" = $'[ignore]\nnew/' ]
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

@test "pull: leaf-scoped pull handles a remote type change from blob to tree" {
    # Regression (Bug1): `omemfs pull <leaf>` used to crash with "tree object
    # hash mismatch" when the remote replaced a plain file with a directory of
    # the same name, because the scoped-pull is_leaf check required BOTH sides
    # to be non-Tree and then diffed the two navigated hashes directly -- one
    # of which was actually a blob hash being read as a tree object.
    setup_two_clones

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -f file.txt ]
    cd ..

    cd clone1
    rm file.txt
    mkdir file.txt
    echo "inner" > file.txt/inner.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull file.txt
    [ "$status" -eq 0 ]
    [ -d file.txt ]
    [ -f file.txt/inner.txt ]
    [ "$(cat file.txt/inner.txt)" = "inner" ]
}

@test "pull: leaf-scoped pull handles a remote type change from tree to blob" {
    # Regression (Bug1), reverse direction: the remote replaces a directory
    # with a plain file of the same name.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir d
    echo "inner" > d/inner.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -d d ]
    cd ..

    cd clone1
    rm -rf d
    echo "now a file" > d
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull d
    [ "$status" -eq 0 ]
    [ -f d ]
    [ "$(cat d)" = "now a file" ]
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

@test "pull: whole-stubs a newly-pulled directory that is itself a git repo root" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p gitproject
    git -C gitproject init -q
    # Large file (above threshold 10) inside a git working tree, pushed as
    # part of the remote tree including .git itself.
    printf '%020d' 0 > gitproject/large.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2 has no local gitproject at all, so the entry is newly added from
    # pull's diff perspective (base_tree_hash is None). It must be
    # whole-stubbed, not materialised, even though it is a git repo root --
    # see design/08_stub_system.md "Stubs and Git repositories" and
    # src/commands/clone.rs's expand_or_stub for the reference behaviour
    # omemfs pull must match.
    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    [ -f gitproject/.omemfs-stub ]
    [ ! -f gitproject/large.txt ]
    [ ! -e gitproject/.git ]

    # expand restores the repo intact, including the file.
    run "$OMEMFS" expand gitproject
    [ "$status" -eq 0 ]
    [ -f gitproject/large.txt ]
    [ ! -f gitproject/.omemfs-stub ]
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

@test "pull: single-file scoped pull writes conflict helpers beside the leaf" {
    setup_two_clones

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "local change" > file.txt

    cd ../clone1
    echo "remote change" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull file.txt
    [ "$status" -ne 0 ]
    [ "$(cat file.txt)" = "local change" ]
    [ -f file.txt.omemfs-conflict-base ]
    [ -f file.txt.omemfs-conflict-local ]
    [ -f file.txt.omemfs-conflict-remote ]
    [ "$(cat file.txt.omemfs-conflict-base)" = "hello" ]
    [ "$(cat file.txt.omemfs-conflict-local)" = "local change" ]
    [ "$(cat file.txt.omemfs-conflict-remote)" = "remote change" ]
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

@test "pull: stale omemfs-conflict-remote from an earlier conflict is cleared when the remote side later has no content (D8)" {
    setup_two_clones

    # clone2: sync file.txt into clone_root, then locally modify it.
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "local change" > file.txt

    # clone1: modify file.txt and push -- clone2's next pull conflicts with a
    # real, non-empty remote side, so a real .omemfs-conflict-remote helper
    # gets written this first time.
    cd ../clone1
    echo "remote change v1" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [ -f file.txt.omemfs-conflict-remote ]
    [ "$(cat file.txt.omemfs-conflict-remote)" = "remote change v1" ]

    # Leave the conflict unresolved. clone1 now deletes file.txt and pushes --
    # the remote side of the *same* still-unresolved conflict has changed
    # shape from a blob to an absence.
    cd ../clone1
    rm file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # clone2 pulls again. The remote side is now deleted, so no
    # .omemfs-conflict-remote helper should exist for this re-detected
    # conflict -- the stale v1 helper from the first conflict must be
    # cleared, not left behind (design/03_sync_model.md, "Missing sides",
    # "Clearing stale residue before writing").
    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [ ! -f file.txt.omemfs-conflict-remote ]

    # accept-remote must delete the file (remote has no content), not
    # resurrect the stale v1 content.
    run "$OMEMFS" conflict accept-remote file.txt
    [ "$status" -eq 0 ]
    [ ! -f file.txt ]
}

@test "pull: stale omemfs-conflict-base is refreshed when clone_root's own recorded value for the path changes across two unresolved conflicts (D8)" {
    setup_two_clones

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "local change v1" > file.txt

    cd ../clone1
    echo "remote change v1" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [ "$(cat file.txt.omemfs-conflict-base)" = "hello" ]

    # Resolve nothing; instead simulate clone_root itself having advanced by
    # directly editing local content again so a second, distinct conflict
    # re-detects on the same path -- exercising that the base helper is
    # rewritten fresh each time, never merely left over from before.
    echo "local change v2" > file.txt
    cd ../clone1
    echo "remote change v2" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [ "$(cat file.txt.omemfs-conflict-base)" = "hello" ]
    [ "$(cat file.txt.omemfs-conflict-remote)" = "remote change v2" ]
    [ "$(cat file.txt.omemfs-conflict-local)" = "local change v2" ]
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

@test "pull: leaf-scoped pull of a stubbed file applies a remote update via stub reconcile, not a conflict" {
    # Regression (Bug2): 'omemfs pull <file>' misreported a stubbed leaf's
    # absence on disk as a local deletion, so a normal remote content update
    # to that same path was reported as a conflict even though the local
    # side made no change of its own (the file was merely stubbed).
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    printf '%020d' 0 > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 clone2
    [ "$status" -eq 0 ]
    [ -f clone2/big.bin.omemfs-stub ]
    [ ! -f clone2/big.bin ]
    STUB_BEFORE="$(cat clone2/big.bin.omemfs-stub)"

    cd clone1
    printf '%020d' 9 > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull big.bin --stub-threshold 10
    [ "$status" -eq 0 ]
    [[ "$output" != *"Conflict"* ]]
    [ -f big.bin.omemfs-stub ]
    [ ! -f big.bin ]
    STUB_AFTER="$(cat big.bin.omemfs-stub)"
    [ "$STUB_BEFORE" != "$STUB_AFTER" ]
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
# D7: pull performs stub reconcile (design/08_stub_system.md, "Stub reconcile
# (pull and restore)"). Every existing stub in the working tree must be
# checked against the new clone_root after apply_diff/write_clone_root, and
# updated/removed/switched-kind per the reconcile table -- not just left with
# whatever record it had before the pull.
# ---------------------------------------------------------------------------

@test "pull: reconcile updates a dir stub's fields when its remote content changes underneath it" {
    # A dir-stubbed subtree's own record is not touched by ordinary diff/apply
    # (the diff never descends into a dir-stubbed path -- see
    # plan_reconcile_dir_stub_to_tree's restore.rs analogue). Reconcile must
    # still bring the stub's hash/size/mtime/blob_count in line with the new
    # clone_root even though no per-file diff entry for anything under
    # bigdir/ is ever produced against a dir-stubbed local path.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p bigdir
    printf '%020d' 0 > bigdir/a.txt
    printf '%020d' 1 > bigdir/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 clone2
    [ "$status" -eq 0 ]
    [ -f clone2/bigdir/.omemfs-stub ]
    STUB_BEFORE="$(cat clone2/bigdir/.omemfs-stub)"

    # clone1: modify a file inside bigdir and add another, then push.
    cd clone1
    printf '%020d' 9 > bigdir/a.txt
    echo "extra" > bigdir/c.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    # Still a whole directory stub -- reconcile never recurses into it.
    [ -f bigdir/.omemfs-stub ]
    [ ! -f bigdir/a.txt ]
    [ ! -f bigdir/c.txt ]
    STUB_AFTER="$(cat bigdir/.omemfs-stub)"
    [ "$STUB_BEFORE" != "$STUB_AFTER" ]
    # Expanding must produce the new content, proving the stub's hash was
    # actually advanced to the new clone_root, not left stale.
    run "$OMEMFS" expand bigdir
    [ "$status" -eq 0 ]
    [ "$(cat bigdir/a.txt)" = "$(printf '%020d' 9)" ]
    [ "$(cat bigdir/c.txt)" = "extra" ]
}

@test "pull: reconcile removes a file stub whose path is deleted on the remote via an unrelated pull" {
    # Same delete-reconcile guarantee as the existing dir-stub deletion test
    # above, but for a FILE stub, and triggered by a pull whose own diff does
    # not otherwise touch the stubbed path (the deletion happened earlier;
    # this pull's remote diff is for an unrelated file only). Reconcile must
    # still catch the now-absent stub on every pull, not just the one whose
    # diff happens to mention it.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    printf '%020d' 0 > big.bin
    # Kept well under the 10-byte stub threshold used below, so this file is
    # never itself a stub candidate -- only big.bin is.
    echo "x" > other.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 clone2
    [ "$status" -eq 0 ]
    [ -f clone2/big.bin.omemfs-stub ]
    [ ! -f clone2/other.txt.omemfs-stub ]

    # clone1 deletes big.bin and, separately, modifies other.txt; both in one push.
    cd clone1
    rm big.bin
    echo "y" > other.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    [ ! -f big.bin.omemfs-stub ]
    [ ! -f big.bin ]
    [ "$(cat other.txt)" = "y" ]
}

@test "pull: reconcile switches a file stub to a dir stub when the remote path becomes a directory" {
    # design/08 reconcile table: clone_root entry = tree, stub kind = file
    # stub -> remove the file stub, create the directory, write a dir stub
    # inside. This must happen even when the type-changing path itself is
    # small enough that ordinary diff/apply already handles the type change
    # (this test also exercises reconcile's ability to run after a normal
    # AddedTree/Deleted apply, not just as a substitute for it).
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    printf '%020d' 0 > thing
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 clone2
    [ "$status" -eq 0 ]
    [ -f clone2/thing.omemfs-stub ]
    [ ! -e clone2/thing ]

    cd clone1
    rm thing
    mkdir thing
    printf '%020d' 1 > thing/x.txt
    printf '%020d' 2 > thing/y.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    [ ! -f thing.omemfs-stub ]
    [ -d thing ]
    [ -f thing/.omemfs-stub ]
    run "$OMEMFS" expand thing
    [ "$status" -eq 0 ]
    [ "$(cat thing/x.txt)" = "$(printf '%020d' 1)" ]
    [ "$(cat thing/y.txt)" = "$(printf '%020d' 2)" ]
}

@test "pull: reconcile switches a file stub to a dir stub even when the new tree is too small to auto-stub" {
    # Regression: unlike the previous test (where the new tree meets
    # --stub-threshold and the diff arrives as a single AddedTree), a SMALL
    # new tree is diffed recursively into ordinary Deleted(f.txt) +
    # Added(f.txt/inner.txt) entries. apply_diff's Deleted-entry phase
    # previously removed the file stub marker unconditionally before
    # anything recognised this was a type change, so the subsequent
    # Added(f.txt/inner.txt) materialised inner.txt as an ordinary real file
    # under f.txt/ instead of leaving the whole path to reconcile's
    # "tree | file stub -> SwitchToDirStub" row. By the time reconcile_stubs
    # ran, the stub record was already gone, so nothing caught the mistake.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    echo "content" > f.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    run "$OMEMFS" stub f.txt
    [ "$status" -eq 0 ]
    [ -f f.txt.omemfs-stub ]
    [ ! -e f.txt ]
    cd ..

    # clone1: replace f.txt with a small directory (well under the default
    # 1 MiB stub threshold) and push.
    cd clone1
    rm f.txt
    mkdir f.txt
    echo "inner content" > f.txt/inner.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2: pull with the DEFAULT stub threshold (no --stub-threshold
    # override) -- the new tree is small, so ordinary diff/apply would
    # normally materialise it, but this path was a file stub and must
    # become a directory stub instead, never a real materialised file.
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ ! -f f.txt.omemfs-stub ]
    [ -d f.txt ]
    [ -f f.txt/.omemfs-stub ]
    [ ! -f f.txt/inner.txt ]
    run "$OMEMFS" expand f.txt
    [ "$status" -eq 0 ]
    [ "$(cat f.txt/inner.txt)" = "inner content" ]
}

@test "pull: reconcile switches a dir stub to a file stub when the remote path becomes a file" {
    # design/08 reconcile table: clone_root entry = blob, stub kind = dir
    # stub -> remove the dir stub marker, remove the directory, write a file
    # stub in its place.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p bigdir
    printf '%020d' 0 > bigdir/a.txt
    printf '%020d' 1 > bigdir/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 clone2
    [ "$status" -eq 0 ]
    [ -f clone2/bigdir/.omemfs-stub ]

    cd clone1
    rm -rf bigdir
    printf '%020d' 5 > bigdir
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    [ ! -d bigdir ]
    [ -f bigdir.omemfs-stub ]
    run "$OMEMFS" expand bigdir
    [ "$status" -eq 0 ]
    [ "$(cat bigdir)" = "$(printf '%020d' 5)" ]
}

@test "pull: reconcile replaces a dir stub with a materialised symlink when the remote path becomes a symlink" {
    # design/08 reconcile table: clone_root entry = symlink, any stub kind ->
    # remove the stub, create the symlink (symlinks are always materialised).
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p bigdir
    printf '%020d' 0 > bigdir/a.txt
    printf '%020d' 1 > bigdir/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 clone2
    [ "$status" -eq 0 ]
    [ -f clone2/bigdir/.omemfs-stub ]

    cd clone1
    rm -rf bigdir
    ln -s /somewhere bigdir
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    [ ! -f bigdir/.omemfs-stub ]
    [ ! -f bigdir.omemfs-stub ]
    [ -L bigdir ]
    [ "$(readlink bigdir)" = "/somewhere" ]
}

@test "pull: reconcile leaves a partially-expanded directory's real files untouched, updating only the stub record" {
    # design/08 "Partial expansion": a directory stub coexisting with real
    # (locally added) files must have ONLY its .omemfs-stub record updated by
    # reconcile; the real files are never touched, and reconcile must not
    # recurse into (or otherwise disturb) the directory beyond that record.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p sub
    printf '%020d' 0 > sub/a.txt
    printf '%020d' 1 > sub/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 clone2
    [ "$status" -eq 0 ]
    [ -f clone2/sub/.omemfs-stub ]

    # Partial expansion: add a new real file alongside the stub marker.
    cd clone2
    echo "local-added" > sub/local.txt
    STUB_BEFORE="$(cat sub/.omemfs-stub)"
    cd ..

    # clone1: change a.txt (part of the stub's own recorded tree) and push.
    cd clone1
    printf '%020d' 9 > sub/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    # The real, locally-added file must be completely untouched.
    [ "$(cat sub/local.txt)" = "local-added" ]
    # The stub record itself must have advanced (new a.txt hash pulled in).
    [ -f sub/.omemfs-stub ]
    STUB_AFTER="$(cat sub/.omemfs-stub)"
    [ "$STUB_BEFORE" != "$STUB_AFTER" ]
    # b.txt is still only in the stub record, not materialised.
    [ ! -f sub/b.txt ]
}

@test "pull: working-tree scan merges a partial expansion whose stub tree was never fetched by a lazy clone (D15)" {
    # design/08 "Partial expansion" + "Stub threshold": a directory whole-
    # stubbed by a lazy clone never has its tree object fetched. Adding a
    # real file directly inside the stub marker, then pulling an UNRELATED
    # remote change, must not crash: pull's own working-tree scan (which runs
    # before the diff) has to merge the stub's never-locally-cached recorded
    # entries with the on-disk file, fetching the stub's tree object from the
    # remote on demand.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p bigdir
    printf '%020d' 0 > bigdir/a.txt
    printf '%020d' 1 > bigdir/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 clone2
    [ "$status" -eq 0 ]
    [ -f clone2/bigdir/.omemfs-stub ]

    cd clone2
    echo "new-c" > bigdir/c.txt
    cd ..

    # clone1: push an unrelated change so clone2's pull has to run its
    # working-tree scan (a pull that ends up "already up to date" would
    # short-circuit before ever reaching the scan). Kept well below the 10
    # byte --stub-threshold used below so this file itself is never stubbed.
    cd clone1
    echo -n "abc" > other.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    [[ "$output" != *"object not found"* ]]
    [ -f other.txt ]
    # The local addition must be preserved.
    [ "$(cat bigdir/c.txt)" = "new-c" ]
}

@test "pull: deleting one file inside a dir-stubbed subtree on the remote does not remove the whole directory stub" {
    # Regression: apply_diff's post-delete cleanup pass previously removed the
    # ENTIRE directory stub whenever the working tree showed only the
    # .omemfs-stub marker after processing Deleted entries -- but that marker
    # is also what a directory stub with SOME (not all) of its logical
    # content deleted on the remote looks like on disk. Only reconcile
    # (consulting the new clone_root) can tell a whole-subtree deletion apart
    # from a partial one; deleting just bigdir/a.txt on the remote must leave
    # bigdir/.omemfs-stub in place (updated to the new, smaller tree), not
    # remove bigdir entirely.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p bigdir
    printf '%020d' 0 > bigdir/a.txt
    printf '%020d' 1 > bigdir/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 clone2
    [ "$status" -eq 0 ]
    [ -f clone2/bigdir/.omemfs-stub ]

    # clone1: delete only ONE of the two files, keep the directory (and the
    # other file) alive on the remote.
    cd clone1
    rm bigdir/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    # bigdir must still exist as a directory stub -- not removed entirely.
    [ -d bigdir ]
    [ -f bigdir/.omemfs-stub ]
    run "$OMEMFS" expand bigdir
    [ "$status" -eq 0 ]
    [ ! -f bigdir/a.txt ]
    [ -f bigdir/b.txt ]
    [ "$(cat bigdir/b.txt)" = "$(printf '%020d' 1)" ]
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
    # Scoped to a subdirectory so this exercises the pd.rel-prefixed
    # diff-map-key join that pull_scoped's per-path loop uses for nested
    # paths (see src/commands/pull.rs pull_scoped, `format!("{}/{}", pd.rel,
    # path)`). The bare-filename leaf-scope case (e.g. `pull .zsh_history` at
    # the repo root) is covered separately below.
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

    # Push scoped to "logs", matching what was actually pulled/merged.
    # clone2 has only ever done scoped pulls of "logs", so clone_root never
    # recorded the repo root's other entries (e.g. the default
    # `.omemfs-filter` clone --new creates); an unscoped push would compare
    # the whole tree against remote and spuriously look diverged there even
    # though nothing outside "logs" is actually at risk (design/03_sync_model.md,
    # "Push → Steps" step 4b applies to the whole tree for a full push).
    run "$OMEMFS" push logs
    [ "$status" -eq 0 ]
}

@test "pull: line-merges .zsh_history via a bare-filename leaf scope" {
    # Regression: unlike the subdirectory-scoped case above, a bare leaf-file
    # scope target (`pull .zsh_history` at the repo root) resolves its diff
    # via the leaf-scope wrapper tree path in pull_scoped, not the
    # pd.rel-prefixed nested-path join. Line merging must still succeed here.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    printf ': 100:0;cmd_100\n' > .zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    cd ../clone2
    run "$OMEMFS" pull .zsh_history
    [ "$status" -eq 0 ]
    [ "$(cat .zsh_history)" = ": 100:0;cmd_100" ]
    printf ': 200:0;cmd_local\n' >> .zsh_history

    cd ../clone1
    printf ': 300:0;cmd_remote\n' >> .zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # clone2: bare-filename scoped pull should merge cleanly, not conflict.
    cd ../clone2
    run "$OMEMFS" pull .zsh_history
    [ "$status" -eq 0 ]
    [[ "$output" == *"erged"* ]]
    [ ! -f .zsh_history.omemfs-conflict-base ]
    [ ! -f .zsh_history.omemfs-conflict-local ]
    [ ! -f .zsh_history.omemfs-conflict-remote ]
    grep -q "cmd_100" .zsh_history
    grep -q "cmd_local" .zsh_history
    grep -q "cmd_remote" .zsh_history

    # Push scoped to ".zsh_history", matching what was actually pulled/merged
    # (same rationale as the subdirectory-scoped variant above).
    run "$OMEMFS" push .zsh_history
    [ "$status" -eq 0 ]
}

@test "pull: line-merges .zsh_history without splitting a backslash-continued multi-line command" {
    # D17 regression: a zsh extended_history entry for a multi-line command
    # (e.g. a `for` loop entered interactively) is written as several
    # PHYSICAL lines, every one but the last ending in `\`. Only the first
    # physical line carries the ": <epoch>:<duration>;" timestamp prefix; the
    # continuation lines have none. Before the fix, line_merge split on bare
    # `\n` and treated each physical line as an independent record, so the
    # continuation lines (no timestamp -> sort key i64::MIN) floated to the
    # front of the merged output, detached from the loop's own header line
    # and from each other -- silently corrupting the multi-line command.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    # clone1: establish a shared base and push it.
    cd clone1
    printf ': 100:0;echo start\n' > .zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # clone2: pull the base, then locally append a multi-line `for` loop
    # entry -- 3 physical lines, the first 2 ending in a literal backslash.
    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    printf ': 200:0;for i in 1 2 3; do \\\necho $i\\\ndone\n' >> .zsh_history

    # clone1: append a remote-only entry and push, so clone2's next pull has
    # both a local-only and a remote-only change -- the clean-merge case.
    cd ../clone1
    printf ': 300:0;echo end\n' >> .zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # clone2: pull should merge cleanly, and the for-loop's 3 physical lines
    # must survive intact as one contiguous block.
    cd ../clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [[ "$output" == *"erged"* ]]
    [ ! -f .zsh_history.omemfs-conflict-base ]
    [ ! -f .zsh_history.omemfs-conflict-local ]
    [ ! -f .zsh_history.omemfs-conflict-remote ]

    # The exact 3-line block, byte for byte, contiguous, in this order.
    python3 - .zsh_history <<'PY'
import sys
content = open(sys.argv[1], "rb").read()
block = b"for i in 1 2 3; do \\\necho $i\\\ndone"
assert block in content, f"for-loop block not found intact: {content!r}"
PY

    # "done" must immediately follow "echo $i\" (not detached/reordered) --
    # checked again independently of the python block check above via plain
    # line adjacency.
    local done_line_no echo_line_no
    echo_line_no="$(grep -n '^echo \$i\\$' .zsh_history | head -1 | cut -d: -f1)"
    done_line_no="$(grep -n '^done$' .zsh_history | head -1 | cut -d: -f1)"
    [ -n "$echo_line_no" ]
    [ -n "$done_line_no" ]
    [ "$((echo_line_no + 1))" -eq "$done_line_no" ]

    grep -q "echo start" .zsh_history
    grep -q "echo end" .zsh_history

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

# ---------------------------------------------------------------------------
# Reserved-name rejection in remote tree entries (design/09_reserved_names.md,
# "Reserved names in remote tree entries (untrusted input)"; design/01
# "Invariants" #5). A remote that has been tampered with (or a hostile
# collaborator with write access) could publish a tree whose root entry is
# named ".omemfs" or "<name>.omemfs-stub" -- `pull` must refuse to
# materialise it via `Tree::deserialise`'s reserved-name check, rather than
# writing an entry that would overwrite the local metadata directory or be
# misread as a genuine stub marker.
# ---------------------------------------------------------------------------

# Hand-craft a minimal well-formed tree object with a single blob entry named
# $2, register it in a synthetic delta index file (src/codec/pack/index.rs
# format: ED E2 header + one Inline entry carrying the tree's stored bytes),
# prepend that delta to INDEX_ROOT's delta_hashes, and set remote_root to the
# tree's hash (design/02_storage_format.md "INDEX_ROOT (index root object)")
# -- so a subsequent `pull`/`clone --existing` resolves this crafted tree as
# the remote root through the normal pack-index read path (`PackReader::
# locate`), exactly like a real pushed tree would be. Writing the tree object
# bytes directly under objects/ without an index entry is NOT enough: reads
# go through the delta/hot/cold index (src/codec/pack/reader.rs `locate`),
# which never falls back to a raw objects/ scan. Assumes an unencrypted
# remote (INDEX_ROOT is plaintext) with no existing cold shards beyond the
# default single all-zero slot (true for a fresh `clone --new` remote, as
# used by every caller of this helper).
craft_malicious_remote_root() {
    local remote_dir="$1" entry_name="$2"
    python3 - "$remote_dir" "$entry_name" <<'PY'
import hashlib, json, os, struct, sys

remote_dir, entry_name = sys.argv[1], sys.argv[2]

# Stage 1 (serialize) tag ED F1 (src/object.rs TYPE_TAG_TREE).
entry = {"kind": "blob", "name": entry_name, "hash": "a" * 64, "mtime": None, "size": 0}
tree_json = json.dumps({"kind": "normal", "entries": [entry]}, separators=(",", ":")).encode()
tree_serialised = bytes([0xED, 0xF1]) + tree_json
tree_hash = hashlib.sha256(tree_serialised).digest()
assert len(tree_serialised) < 256, "tree must fit an Inline index entry (1-byte data_length)"

def obj_path(root, hex_hash):
    return os.path.join(root, "objects", hex_hash[0:2], hex_hash[2:4], hex_hash[4:6], hex_hash[6:])

def write_obj(root, hex_hash, data):
    p = obj_path(root, hex_hash)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "wb") as f:
        f.write(data)

# Delta index file: ED E2, version 1, reserved 0, entry_count=1 (big-endian),
# then one Inline entry: tag 0x01, hash (32B), data_length (1B), data.
inline_entry = bytes([0x01]) + tree_hash + bytes([len(tree_serialised)]) + tree_serialised
delta_bytes = bytes([0xED, 0xE2, 0x01, 0x00]) + struct.pack(">I", 1) + inline_entry
delta_hash = hashlib.sha256(delta_bytes).digest()
write_obj(remote_dir, delta_hash.hex(), delta_bytes)

# Rewrite INDEX_ROOT (design/02_storage_format.md "INDEX_ROOT (index root
# object)" plaintext layout): keep hot_hash / bloom_hash / cold_shards as-is,
# set remote_root to our tree hash, and prepend our delta hash.
root_path = os.path.join(remote_dir, "INDEX_ROOT")
with open(root_path, "rb") as f:
    raw = f.read()
assert raw[0:2] == bytes([0xED, 0xE3]), "unencrypted INDEX_ROOT expected"
version = raw[2]
hot_hash = raw[36:68]
bloom_hash = raw[68:100]
cold_prefix_bits = raw[100]
delta_count_old = struct.unpack(">H", raw[104:106])[0]
pos = 108
old_delta_hashes = [raw[pos + i * 32:pos + i * 32 + 32] for i in range(delta_count_old)]
pos += delta_count_old * 32
cold_shards = raw[pos:pos + (1 << cold_prefix_bits) * 32]

new_delta_hashes = [delta_hash] + old_delta_hashes
out = bytearray()
out += bytes([0xED, 0xE3, version, 0x00])
out += tree_hash
out += hot_hash
out += bloom_hash
out += bytes([cold_prefix_bits]) + bytes(3)
out += struct.pack(">H", len(new_delta_hashes))
out += bytes(2)
for dh in new_delta_hashes:
    out += dh
out += cold_shards
with open(root_path, "wb") as f:
    f.write(out)
PY
}

@test "pull: refuses a remote root tree with a .omemfs entry name" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    cd clone1
    echo "seed" > seed.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    craft_malicious_remote_root "$REMOTE_DIR" ".omemfs"

    cd clone1
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [[ "$output" == *"invalid object"* ]]
    # The local metadata directory must be untouched -- no ".omemfs" entry
    # was ever written into the working tree as a regular file/tree.
    [ -d .omemfs ]
    [ -f .omemfs/clone_root ]
}

@test "pull: refuses a remote root tree with a fake .omemfs-stub entry name" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    cd clone1
    echo "seed" > seed.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    craft_malicious_remote_root "$REMOTE_DIR" "victim.txt.omemfs-stub"

    cd clone1
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [[ "$output" == *"invalid object"* ]]
    [ ! -f victim.txt.omemfs-stub ]
}

# ---------------------------------------------------------------------------
# D6 regression: remote-side type change (tree <-> blob) must not leave pull
# permanently failing (design/03_sync_model.md, "Diff algorithm" -- a type
# change must emit deleted(path) for the old side's full content, in addition
# to added(path) for the new side).
# ---------------------------------------------------------------------------

@test "pull: remote directory replaced by a file pulls successfully and replaces the old files" {
    # clone1 turns a.txt (a directory containing files) into a plain file, then
    # pushes. clone2's pull must delete the old directory's files and write the
    # new file's content, instead of failing with "Is a directory".
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p a.txt
    echo "child1" > a.txt/child1.txt
    echo "child2" > a.txt/child2.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -d a.txt ]
    [ -f a.txt/child1.txt ]
    [ -f a.txt/child2.txt ]
    cd ..

    # clone1: replace the directory with a plain file and push.
    cd clone1
    rm -rf a.txt
    echo "now a file" > a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2: pull must succeed, remove the old directory's files, and write
    # the new file's content -- not fail with an I/O error ("Is a directory").
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -f a.txt ]
    [ "$(cat a.txt)" = "now a file" ]
    [ ! -e a.txt/child1.txt ]
    [ ! -e a.txt/child2.txt ]
}

@test "pull: remote file replaced by a directory pulls successfully and removes the old file" {
    # Reverse direction: a.txt starts as a plain file, then clone1 replaces it
    # with a directory of files. clone2's pull must delete the old file and
    # materialise the new directory's contents.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    echo "was a file" > a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -f a.txt ]
    cd ..

    # clone1: replace the file with a directory and push.
    cd clone1
    rm -f a.txt
    mkdir -p a.txt
    echo "child1" > a.txt/child1.txt
    echo "child2" > a.txt/child2.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone2: pull must succeed, remove the old file, and materialise the new
    # directory -- not fail with "refusing to write through a symlinked
    # directory" or leave the stale file in place.
    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -d a.txt ]
    [ -f a.txt/child1.txt ]
    [ -f a.txt/child2.txt ]
    [ "$(cat a.txt/child1.txt)" = "child1" ]
    [ "$(cat a.txt/child2.txt)" = "child2" ]
}

@test "pull: remote file replaced by a large directory (auto-stubbed) pulls successfully and removes the old file" {
    # D7-followup regression: diff_recursive's Tree branch first inserted
    # Deleted(path) for the old blob, then -- when the new tree meets
    # stub_threshold -- inserted AddedTree at the SAME key `path`, silently
    # overwriting the Deleted entry (a plain HashMap::insert has no way to
    # keep both). apply_diff's Phase 2a (delete-first pass) never saw the
    # deletion, so the stale file was still on disk when Phase 2b tried to
    # create_dir_all + write the dir stub there, failing with "refusing to
    # write through a symlinked directory" (fsguard's ENOTDIR/ELOOP guard
    # tripping on a non-directory in the way).
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    echo "was a file" > thing
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -f thing ]
    cd ..

    # clone1: replace the file with a directory large enough to be auto-stubbed.
    cd clone1
    rm -f thing
    mkdir -p thing
    printf '%020d' 1 > thing/x.txt
    printf '%020d' 2 > thing/y.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    [ ! -f thing ]
    [ -d thing ]
    [ -f thing/.omemfs-stub ]
}

@test "pull: remote symlink replaced by a large directory (auto-stubbed) pulls successfully and removes the old symlink" {
    # Same HashMap-key-collision bug as above, but for the Symlink -> Tree
    # type-change direction (diff_recursive's Tree branch treats a base
    # Symlink the same as a base Blob).
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    ln -s /nonexistent thing
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -L thing ]
    cd ..

    cd clone1
    rm -f thing
    mkdir -p thing
    printf '%020d' 1 > thing/x.txt
    printf '%020d' 2 > thing/y.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -eq 0 ]
    [ ! -L thing ]
    [ -d thing ]
    [ -f thing/.omemfs-stub ]
}

@test "pull: remote AddedTree (auto-stubbed new directory) conflicts with a local untracked file under the same name" {
    # classify_conflicts compared remote_diff and local_diff keys by plain
    # string equality. A remote directory large enough to be auto-stubbed is
    # reported as a single AddedTree at the directory's own path ("d"), while
    # local_diff (always computed with stub_threshold=0) reports an untracked
    # file under that not-yet-existing directory at the child path ("d/x").
    # Neither map has the other's exact key, so the collision went undetected
    # and pull applied the AddedTree straight over the untracked file.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    # clone2: create an untracked file under a directory name that does not
    # exist in clone_root yet -- before pulling.
    cd clone2
    mkdir -p d
    echo "untracked local content" > d/x.txt
    cd ..

    # clone1: push a new directory "d" large enough to be auto-stubbed.
    cd clone1
    mkdir -p d
    printf '%020d' 1 > d/a.txt
    printf '%020d' 2 > d/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull --stub-threshold 10
    [ "$status" -ne 0 ]
    [[ "$output" == *"conflict"* ]] || [[ "$output" == *"Conflict"* ]]
    # The untracked file must survive untouched -- pull must not have applied
    # anything (atomic-abort policy).
    [ -f d/x.txt ]
    [ "$(cat d/x.txt)" = "untracked local content" ]
    [ ! -f d/.omemfs-stub ]
}

# ---------------------------------------------------------------------------
# D12: scoped pull must detect a local deletion the same way a full pull does
# ---------------------------------------------------------------------------

@test "pull: local deletion overlapping remote modification is a conflict (single scoped pull)" {
    # Regression (D12): diff_trees(base, target=None) previously returned an
    # empty diff instead of expanding base's content as Deleted, so a scoped
    # pull's local-diff for a path removed entirely from the working tree came
    # back empty. That made classify_conflicts treat the remote change as
    # non-conflicting clean input, silently resurrecting the remote content
    # over a deliberate local delete. A full pull (whose top-level working-tree
    # scan always produces some root hash) never hits this branch, so only the
    # scoped path was affected.
    setup_two_clones

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    rm file.txt
    cd ..

    cd clone1
    echo "remote change" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull file.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"conflict"* ]] || [[ "$output" == *"Conflict"* ]]
    # Must NOT be silently resurrected with the remote content.
    [ ! -f file.txt ]
    [ -f file.txt.omemfs-conflict-remote ]
    [ "$(cat file.txt.omemfs-conflict-remote)" = "remote change" ]
    # Local side had no content (file was deleted): no local helper file.
    [ ! -f file.txt.omemfs-conflict-local ]
}

@test "pull: local deletion overlapping remote modification is a conflict (multi-path scoped pull)" {
    # Same regression as above, exercised through the multi-path scoped pull
    # entry point (pull <path>... with more than one <path>), which builds
    # each pd's local_diff the same way.
    setup_two_clones

    cd clone1
    mkdir -p src
    echo "hello" > src/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    rm -f src/file.txt file.txt
    rmdir src
    cd ..

    cd clone1
    echo "remote change" > file.txt
    echo "remote src change" > src/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull file.txt src
    [ "$status" -ne 0 ]
    [[ "$output" == *"conflict"* ]] || [[ "$output" == *"Conflict"* ]]
    [ ! -f file.txt ]
    [ ! -f src/file.txt ]
    [ -f file.txt.omemfs-conflict-remote ]
    [ -f src/file.txt.omemfs-conflict-remote ]
}

# ---------------------------------------------------------------------------
# D11: multi-path scoped pull must not collide same-named conflicts across
# different scoped subtrees
# ---------------------------------------------------------------------------

@test "pull: multi-path scoped pull reports and writes helpers for same-named conflicts under different paths" {
    # Regression (D11): each path's pd.remote_diff/pd.local_diff use keys
    # relative to that path's own scoped subtree root, so pulling `a` and `b`
    # where both a/f and b/f conflict previously produced the SAME key ("f")
    # in conflict_owner/all_conflicts -- the second path's entry silently
    # overwrote the first's in conflict_owner (a HashMap), and all_conflicts
    # held "f" twice instead of "a/f" and "b/f" once each. As a result the
    # conflict helper files (and the printed "conflict: ..." lines) for one of
    # the two paths were never written / were duplicated, even though both
    # a/f and b/f are genuinely separate conflicts.
    setup_two_clones

    cd clone1
    mkdir -p a b
    echo "base-a" > a/f
    echo "base-b" > b/f
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "local-a" > a/f
    echo "local-b" > b/f
    cd ..

    cd clone1
    echo "remote-a" > a/f
    echo "remote-b" > b/f
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull a b
    [ "$status" -ne 0 ]

    # Both conflicts must be reported, each under its own repo-relative path
    # -- not collapsed into a single "f" (and not just one of the two).
    [[ "$output" == *"a/f"* ]]
    [[ "$output" == *"b/f"* ]]

    # Both paths keep their own local content (atomic abort — nothing applied).
    [ "$(cat a/f)" = "local-a" ]
    [ "$(cat b/f)" = "local-b" ]

    # Each path gets its own correct set of helper files, with the right
    # content for that path -- not the other path's content, and not missing.
    [ -f a/f.omemfs-conflict-base ]
    [ -f a/f.omemfs-conflict-local ]
    [ -f a/f.omemfs-conflict-remote ]
    [ "$(cat a/f.omemfs-conflict-base)" = "base-a" ]
    [ "$(cat a/f.omemfs-conflict-local)" = "local-a" ]
    [ "$(cat a/f.omemfs-conflict-remote)" = "remote-a" ]

    [ -f b/f.omemfs-conflict-base ]
    [ -f b/f.omemfs-conflict-local ]
    [ -f b/f.omemfs-conflict-remote ]
    [ "$(cat b/f.omemfs-conflict-base)" = "base-b" ]
    [ "$(cat b/f.omemfs-conflict-local)" = "local-b" ]
    [ "$(cat b/f.omemfs-conflict-remote)" = "remote-b" ]
}

@test "pull: conflict list after a multi-path scoped pull shows both same-named conflicts distinctly" {
    # Same setup as above, but checks `omemfs conflict list` afterwards --
    # the persisted conflict-helper state must also distinguish a/f from b/f.
    setup_two_clones

    cd clone1
    mkdir -p a b
    echo "base-a" > a/f
    echo "base-b" > b/f
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "local-a" > a/f
    echo "local-b" > b/f
    cd ..

    cd clone1
    echo "remote-a" > a/f
    echo "remote-b" > b/f
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull a b
    [ "$status" -ne 0 ]

    run "$OMEMFS" conflict list
    [ "$status" -eq 0 ]
    [[ "$output" == *"a/f"* ]]
    [[ "$output" == *"b/f"* ]]
}
