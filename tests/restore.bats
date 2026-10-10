#!/usr/bin/env bats
# Integration tests for `omemfs restore`.

load test_helper/common

setup() {
    setup_repo
    # Populate remote with initial files.
    echo "hello" > file.txt
    mkdir -p sub
    echo "world" > sub/nested.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# ---------------------------------------------------------------------------
# Basic restore
# ---------------------------------------------------------------------------

@test "restore: modified file is restored to clone_root content" {
    echo "modified" > file.txt
    run "$OMEMFS" restore file.txt
    [ "$status" -eq 0 ]
    [ "$(cat file.txt)" = "hello" ]
}

@test "restore: modified nested file is restored" {
    echo "changed" > sub/nested.txt
    run "$OMEMFS" restore sub/nested.txt
    [ "$status" -eq 0 ]
    [ "$(cat sub/nested.txt)" = "world" ]
}

@test "restore: locally added file is deleted" {
    echo "extra" > extra.txt
    run "$OMEMFS" restore extra.txt
    [ "$status" -eq 0 ]
    [ ! -f extra.txt ]
}

@test "restore: deleted file is recreated from clone_root" {
    rm file.txt
    run "$OMEMFS" restore file.txt
    [ "$status" -eq 0 ]
    [ -f file.txt ]
    [ "$(cat file.txt)" = "hello" ]
}

# ---------------------------------------------------------------------------
# Directory scope
# ---------------------------------------------------------------------------

@test "restore: directory scope restores all descendants" {
    echo "changed" > sub/nested.txt
    echo "new" > sub/extra.txt
    run "$OMEMFS" restore sub
    [ "$status" -eq 0 ]
    [ "$(cat sub/nested.txt)" = "world" ]
    [ ! -f sub/extra.txt ]
}

@test "restore: duplicate '.' path arguments do not abort with a stale-delete I/O error" {
    # Regression: `plan_restore` previously re-ran the full-tree walk once per
    # occurrence of an equivalent path argument, queueing the same
    # Action::DeleteFile for the same locally-added file twice. The second
    # `fs::remove_file` hit a bare ENOENT and aborted the whole Apply phase
    # (exit=1) instead of a clean `omemfs restore . .`.
    echo "extra" > extra.txt
    run "$OMEMFS" restore . .
    [ "$status" -eq 0 ]
    [ ! -f extra.txt ]
}

@test "restore: duplicate directory path arguments do not abort with a stale-delete I/O error" {
    echo "extra" > sub/extra.txt
    run "$OMEMFS" restore sub sub
    [ "$status" -eq 0 ]
    [ ! -f sub/extra.txt ]
}

@test "restore: overlapping '.' and subdirectory arguments do not abort with a stale-delete I/O error" {
    echo "extra" > sub/extra.txt
    run "$OMEMFS" restore . sub
    [ "$status" -eq 0 ]
    [ ! -f sub/extra.txt ]
}

@test "restore: explicit directory deletes locally-added nested files and directories" {
    # design/04 restore: restoring an explicitly named directory restores it to
    # clone_root state, which includes DELETING files/directories locally added
    # under it (a locally-added subtree must not survive).
    echo "added top" > sub/added_top.txt
    mkdir -p sub/newdir/deeper
    echo "deep added" > sub/newdir/deeper/c.txt
    run "$OMEMFS" restore sub
    [ "$status" -eq 0 ]
    # Pre-existing tracked file is restored.
    [ "$(cat sub/nested.txt)" = "world" ]
    # Locally-added file and nested directory under sub/ are gone.
    [ ! -f sub/added_top.txt ]
    [ ! -e sub/newdir ]
}

# ---------------------------------------------------------------------------
# Full restore (no path argument)
# ---------------------------------------------------------------------------

@test "restore: no path restores entire working tree" {
    echo "modified" > file.txt
    echo "changed" > sub/nested.txt
    echo "extra" > added.txt
    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ "$(cat file.txt)" = "hello" ]
    [ "$(cat sub/nested.txt)" = "world" ]
    [ ! -f added.txt ]
}

# ---------------------------------------------------------------------------
# Idempotency
# ---------------------------------------------------------------------------

@test "restore: clean working tree reports nothing to restore" {
    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to restore"* ]]
}

@test "restore: second restore after clean is also nothing to restore" {
    echo "modified" > file.txt
    run "$OMEMFS" restore file.txt
    [ "$status" -eq 0 ]
    run "$OMEMFS" restore file.txt
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to restore"* ]]
}

# ---------------------------------------------------------------------------
# --dry-run
# ---------------------------------------------------------------------------

@test "restore: --dry-run does not modify files" {
    echo "modified" > file.txt
    run "$OMEMFS" restore --dry-run file.txt
    [ "$status" -eq 0 ]
    [ "$(cat file.txt)" = "modified" ]
}

@test "restore: --dry-run reports what would be restored" {
    echo "modified" > file.txt
    run "$OMEMFS" restore --dry-run file.txt
    [ "$status" -eq 0 ]
    [[ "$output" == *"file.txt"* ]]
}

# ---------------------------------------------------------------------------
# Conflict helper file cleanup
# ---------------------------------------------------------------------------

@test "restore: removes conflict helper files for the restored path" {
    echo "modified" > file.txt
    echo "base"   > file.txt.omemfs-conflict-base
    echo "local"  > file.txt.omemfs-conflict-local
    echo "remote" > file.txt.omemfs-conflict-remote

    run "$OMEMFS" restore file.txt
    [ "$status" -eq 0 ]
    [ "$(cat file.txt)" = "hello" ]
    [ ! -f file.txt.omemfs-conflict-base ]
    [ ! -f file.txt.omemfs-conflict-local ]
    [ ! -f file.txt.omemfs-conflict-remote ]
}

@test "restore: full restore removes all conflict helper files" {
    echo "modified" > file.txt
    echo "base"   > file.txt.omemfs-conflict-base
    echo "local"  > file.txt.omemfs-conflict-local
    echo "remote" > file.txt.omemfs-conflict-remote

    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ "$(cat file.txt)" = "hello" ]
    [ ! -f file.txt.omemfs-conflict-base ]
    [ ! -f file.txt.omemfs-conflict-local ]
    [ ! -f file.txt.omemfs-conflict-remote ]
}

@test "restore: removes conflict helper files even when only some exist" {
    # Simulate a conflict where only local and remote helpers were written (no base).
    echo "modified" > file.txt
    echo "local"  > file.txt.omemfs-conflict-local
    echo "remote" > file.txt.omemfs-conflict-remote

    run "$OMEMFS" restore file.txt
    [ "$status" -eq 0 ]
    [ ! -f file.txt.omemfs-conflict-local ]
    [ ! -f file.txt.omemfs-conflict-remote ]
}

# Regression: a same-day fix made `plan_remove_added_files` skip conflict
# helper filenames entirely so a duplicate delete could never abort Apply.
# That excluded conflict helpers from the "unknown file" delete path
# altogether, so a base path with NO restore/delete action of its own (a
# directory, an unchanged symlink, or a locally-added file being deleted
# outright) left its conflict helpers behind forever. The real fix is
# problem 1's NotFound-tolerant delete, which lets `plan_remove_added_files`
# go back to treating conflict helpers as ordinary unknown files.

@test "restore: full restore removes conflict helper files for a locally-added file that gets deleted" {
    echo "added" > orphan.txt
    echo "base"   > orphan.txt.omemfs-conflict-base
    echo "local"  > orphan.txt.omemfs-conflict-local
    echo "remote" > orphan.txt.omemfs-conflict-remote

    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ ! -f orphan.txt ]
    [ ! -f orphan.txt.omemfs-conflict-base ]
    [ ! -f orphan.txt.omemfs-conflict-local ]
    [ ! -f orphan.txt.omemfs-conflict-remote ]
}

@test "restore: full restore removes conflict helper files whose base path is a directory in clone_root" {
    # "sub" is a directory recorded in clone_root; the TreeEntry::Tree branch
    # never inspects conflict helpers for its own path, so this exercises
    # plan_remove_added_files picking them up as unknown files in the parent
    # directory scan instead.
    echo "base"   > sub.omemfs-conflict-base
    echo "local"  > sub.omemfs-conflict-local
    echo "remote" > sub.omemfs-conflict-remote

    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ -d sub ]
    [ ! -f sub.omemfs-conflict-base ]
    [ ! -f sub.omemfs-conflict-local ]
    [ ! -f sub.omemfs-conflict-remote ]
}

@test "restore: full restore removes conflict helper files whose base path is an unchanged symlink" {
    ln -s file.txt link.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "base"   > link.txt.omemfs-conflict-base
    echo "local"  > link.txt.omemfs-conflict-local
    echo "remote" > link.txt.omemfs-conflict-remote

    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ -L link.txt ]
    [ "$(readlink link.txt)" = "file.txt" ]
    [ ! -f link.txt.omemfs-conflict-base ]
    [ ! -f link.txt.omemfs-conflict-local ]
    [ ! -f link.txt.omemfs-conflict-remote ]
}

# ---------------------------------------------------------------------------
# Does not touch clone_root
# ---------------------------------------------------------------------------

@test "restore: clone_root is unchanged after restore" {
    local before
    before="$(get_clone_root)"
    echo "modified" > file.txt
    run "$OMEMFS" restore file.txt
    [ "$status" -eq 0 ]
    [ "$(get_clone_root)" = "$before" ]
}

# ---------------------------------------------------------------------------
# Executable-bit (mode) restoration
# ---------------------------------------------------------------------------

@test "restore: chmod -x only change is repaired" {
    echo "#!/bin/sh" > script.sh
    chmod +x script.sh
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    chmod -x script.sh
    run "$OMEMFS" restore script.sh
    [ "$status" -eq 0 ]
    [[ "$output" == *"restored: script.sh"* ]]
    [ -x script.sh ]
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

@test "restore: content change on executable file restores content and bit" {
    echo "#!/bin/sh" > script.sh
    chmod +x script.sh
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "modified" > script.sh
    chmod -x script.sh
    run "$OMEMFS" restore script.sh
    [ "$status" -eq 0 ]
    [ "$(cat script.sh)" = "#!/bin/sh" ]
    [ -x script.sh ]
}

@test "restore: chmod +x only change on non-executable file is repaired" {
    echo "plain" > plain.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    chmod +x plain.txt
    run "$OMEMFS" restore plain.txt
    [ "$status" -eq 0 ]
    [[ "$output" == *"restored: plain.txt"* ]]
    [ ! -x plain.txt ]
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

# ---------------------------------------------------------------------------
# Stub reconcile during restore
# ---------------------------------------------------------------------------

@test "restore: file stub is preserved (not materialised) when blob is absent from local cache" {
    # Push a large file so the remote has it.
    printf 'x%.0s' {1..100} > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Clone fresh with a small stub threshold so the blob is never downloaded.
    local CLONE2
    CLONE2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 "$CLONE2"
    [ "$status" -eq 0 ]
    [ -f "$CLONE2/big.bin.omemfs-stub" ]
    [ ! -f "$CLONE2/big.bin" ]

    # Restore must not crash even though the blob is absent from local cache.
    cd "$CLONE2"
    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ -f big.bin.omemfs-stub ]
    [ ! -f big.bin ]

    rm -rf "$CLONE2"
}

@test "restore: stale file stub record is corrected on restore" {
    echo "#!/bin/sh" > script.sh
    chmod +x script.sh
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub script.sh
    [ "$status" -eq 0 ]
    [ -f script.sh.omemfs-stub ]

    # Strip the mode field from the stub to simulate a stale record.
    python -c "import json; d=json.load(open('script.sh.omemfs-stub')); d.pop('mode', None); json.dump(d, open('script.sh.omemfs-stub', 'w'))"
    run python -c "import json, sys; d=json.load(open('script.sh.omemfs-stub')); sys.exit(0 if 'mode' not in d else 1)"
    [ "$status" -eq 0 ]

    run "$OMEMFS" restore script.sh
    [ "$status" -eq 0 ]
    # Stub must still be present (not materialised).
    [ -f script.sh.omemfs-stub ]
    [ ! -f script.sh ]
    # Stub must now carry the correct mode.
    run python -c "import json, sys; d=json.load(open('script.sh.omemfs-stub')); sys.exit(0 if d.get('mode') == '755' else 1)"
    [ "$status" -eq 0 ]
}

@test "restore: directory stub is reconciled on restore" {
    mkdir -p subdir
    echo "content" > subdir/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub subdir
    [ "$status" -eq 0 ]
    [ -f subdir/.omemfs-stub ]

    # Corrupt the dir stub's size field to simulate a stale record.
    python -c "import json; d=json.load(open('subdir/.omemfs-stub')); d['size']=99999; json.dump(d, open('subdir/.omemfs-stub', 'w'))"
    run python -c "import json, sys; d=json.load(open('subdir/.omemfs-stub')); sys.exit(0 if d['size'] == 99999 else 1)"
    [ "$status" -eq 0 ]

    run "$OMEMFS" restore subdir
    [ "$status" -eq 0 ]
    [ -f subdir/.omemfs-stub ]
    # Dir stub size must be corrected.
    run python -c "import json, sys; d=json.load(open('subdir/.omemfs-stub')); sys.exit(0 if d['size'] != 99999 else 1)"
    [ "$status" -eq 0 ]
}

@test "restore: file stub for entry absent from clone_root is removed on full restore" {
    # Manually create a stub for a path not in clone_root.
    echo '{"target_type":"blob","hash":"0000000000000000000000000000000000000000000000000000000000000000","size":0,"mtime":null}' \
        > orphan.txt.omemfs-stub
    [ -f orphan.txt.omemfs-stub ]

    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ ! -f orphan.txt.omemfs-stub ]
}

@test "restore: directory stub for entry absent from clone_root is removed on full restore" {
    mkdir -p orphan_dir
    echo '{"target_type":"tree","hash":"0000000000000000000000000000000000000000000000000000000000000000","size":0,"mtime":null,"blob_count":0}' \
        > orphan_dir/.omemfs-stub

    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ ! -d orphan_dir ]
}

# ---------------------------------------------------------------------------
# Fix 3 regression: restore must not recurse into a fully-stubbed directory
# ---------------------------------------------------------------------------

@test "restore: directory stub whose blobs are absent from local cache does not error" {
    # Push a directory with a file.
    mkdir -p bigdir
    printf 'x%.0s' {1..100} > bigdir/big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Create a second clone where the dir is kept as a stub (blob never downloaded).
    local CLONE2
    CLONE2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 "$CLONE2"
    [ "$status" -eq 0 ]
    [ -f "$CLONE2/bigdir/.omemfs-stub" ]
    [ ! -f "$CLONE2/bigdir/big.bin" ]

    # restore on the stubbed directory must succeed without trying to materialise blobs.
    cd "$CLONE2"
    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    # Dir stub must still be present, blob must NOT have been created.
    [ -f bigdir/.omemfs-stub ]
    [ ! -f bigdir/big.bin ]

    rm -rf "$CLONE2"
}

@test "restore: directory stub does not recurse and materialise children on targeted restore" {
    # Push a directory containing a file that is larger than the stub threshold.
    mkdir -p assets
    printf 'x%.0s' {1..100} > assets/data.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local CLONE2
    CLONE2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 "$CLONE2"
    [ "$status" -eq 0 ]
    [ -f "$CLONE2/assets/.omemfs-stub" ]

    cd "$CLONE2"
    # Targeting the stubbed directory specifically must also not crash.
    run "$OMEMFS" restore assets
    [ "$status" -eq 0 ]
    [ -f assets/.omemfs-stub ]
    [ ! -f assets/data.bin ]

    rm -rf "$CLONE2"
}


# ---------------------------------------------------------------------------
# Symlink traversal safety
# ---------------------------------------------------------------------------

@test "restore: full restore removes an added directory symlink without touching its target" {
    local outside
    outside="$(mktemp -d)"
    mkdir -p "$outside/sub"
    echo "keep" > "$outside/victim"
    echo "nested" > "$outside/sub/nested"
    ln -s "$outside" escape

    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ ! -e escape ]
    [ -f "$outside/victim" ]
    [ -f "$outside/sub/nested" ]

    rm -rf "$outside"
}

@test "restore: dry-run does not traverse an added directory symlink" {
    local outside
    outside="$(mktemp -d)"
    echo "keep" > "$outside/victim"
    ln -s "$outside" escape

    run "$OMEMFS" restore --dry-run
    [ "$status" -eq 0 ]
    [ -L escape ]
    [ -f "$outside/victim" ]

    rm escape
    rm -rf "$outside"
}

# ---------------------------------------------------------------------------
# Remote fetch for objects absent from the local cache
# (design/04_cli_spec.md, "omemfs restore" -> Behaviour / Missing objects)
# ---------------------------------------------------------------------------

# Make the configured remote unreachable by moving it out from under its
# configured path, without losing its contents (moved back at the end of
# each test that uses this).
disable_remote() {
    mv "$REMOTE_DIR" "${REMOTE_DIR}.moved"
}

reenable_remote() {
    mv "${REMOTE_DIR}.moved" "$REMOTE_DIR"
}

@test "restore: fetches a blob absent from the local cache" {
    printf 'x%.0s' {1..100} > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Clone fresh with a small stub threshold so the blob is never downloaded.
    local CLONE2
    CLONE2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 "$CLONE2"
    [ "$status" -eq 0 ]
    [ -f "$CLONE2/big.bin.omemfs-stub" ]
    [ ! -f "$CLONE2/big.bin" ]

    cd "$CLONE2"
    # This is the minimal reproduction of the object-not-found incident: a
    # path that is neither a stub nor materialised (ls's "D" state), with the
    # blob never fetched into the local cache in the first place.
    rm big.bin.omemfs-stub
    [ ! -e big.bin ]
    [ ! -e big.bin.omemfs-stub ]

    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ -f big.bin ]
    [ "$(cat big.bin)" = "$(printf 'x%.0s' {1..100})" ]

    rm -rf "$CLONE2"
}

@test "restore: fetches missing chunks when the manifest is cached but chunks are not" {
    # 20 MiB, matching tests/chunk.bats's own large-file setup: FastCDC's cut
    # points are content-dependent (an exponential distribution around
    # CDC_AVG = 4 MiB, src/codec/chunk.rs), so a size only slightly above the
    # average is not a hard guarantee of 2+ chunks and would be flaky; 20 MiB
    # reliably produces a manifest (ED F2) plus 2+ chunk objects (ED F3).
    head -c $((20 * 1024 * 1024)) /dev/urandom > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # stub-threshold 0 expands everything, so the clone fully materialises
    # big.bin: both the manifest and every chunk land in the local cache.
    local CLONE2
    CLONE2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 0 "$CLONE2"
    [ "$status" -eq 0 ]
    [ -f "$CLONE2/big.bin" ]

    cd "$CLONE2"
    rm big.bin

    # Delete exactly one chunk object (ED F3 tag) from the local cache,
    # leaving the manifest (ED F2) in place -- simulates an interrupted
    # transfer that wrote the manifest before all of its chunks (the bug
    # `blob_fully_cached` guards against; a bare `local.exists` would miss
    # this and fail with a bare chunk-hash "object not found").
    local chunk_file=""
    local f magic
    while IFS= read -r -d '' f; do
        magic=$(head -c 2 "$f" | od -An -tx1 | tr -d ' \n')
        if [ "$magic" = "edf3" ]; then
            chunk_file="$f"
            break
        fi
    done < <(find .omemfs/objects -type f -print0)
    [ -n "$chunk_file" ]
    rm "$chunk_file"

    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ -f big.bin ]

    rm -rf "$CLONE2"
}

@test "restore: works offline when everything is cached" {
    echo "second push content" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "local edit" > file.txt

    disable_remote

    # Phase 2's early skip (no `repo.pack_reader()` call when nothing is
    # missing) must make this succeed with the remote entirely gone.
    run "$OMEMFS" restore file.txt
    [ "$status" -eq 0 ]
    [ "$(cat file.txt)" = "second push content" ]

    reenable_remote
}

@test "restore: a fully-cached restore never resolves the remote, even a misconfigured one" {
    # Stronger, more direct proof of the same guarantee as the previous test:
    # `--remote does-not-exist` would fail immediately (a plain config
    # lookup, `src/repo.rs`'s `remote_store`, no I/O needed to fail) the
    # moment `repo.pack_reader()` is called. If Phase 2's early skip were
    # ever bypassed for a fully-cached restore, this would fail with
    # "remote 'does-not-exist' not configured" instead of succeeding.
    echo "second push content" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "local edit" > file.txt

    run "$OMEMFS" restore --remote does-not-exist file.txt
    [ "$status" -eq 0 ]
    [ "$(cat file.txt)" = "second push content" ]
}

@test "restore: --no-remote reports unavailable paths instead of fetching" {
    printf 'x%.0s' {1..100} > big.bin
    # Deliberately below --stub-threshold 10 below, so this clones as a real
    # file (not a stub) and restore's ordinary blob-restore path applies to
    # it, distinct from big.bin's fetch-needed path.
    echo "hi" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local CLONE2
    CLONE2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 "$CLONE2"
    [ "$status" -eq 0 ]
    [ -f "$CLONE2/big.bin.omemfs-stub" ]
    [ -f "$CLONE2/file.txt" ]

    cd "$CLONE2"
    rm big.bin.omemfs-stub
    echo "local edit" > file.txt

    run "$OMEMFS" restore --no-remote
    [ "$status" -ne 0 ]
    [[ "$output" == *"unavailable:"* ]]
    [[ "$output" == *"big.bin"* ]]
    [[ "$output" == *"restored: file.txt"* ]]
    [ "$(cat file.txt)" = "hi" ]
    [ ! -f big.bin ]

    rm -rf "$CLONE2"
}

@test "restore: unreachable remote leaves the working tree with every applicable action" {
    printf 'x%.0s' {1..100} > big.bin
    # Deliberately below --stub-threshold 10 below; see the previous test.
    echo "hi" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local CLONE2
    CLONE2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 "$CLONE2"
    [ "$status" -eq 0 ]
    [ -f "$CLONE2/file.txt" ]

    cd "$CLONE2"
    rm big.bin.omemfs-stub
    echo "local edit" > file.txt

    disable_remote

    run "$OMEMFS" restore
    [ "$status" -ne 0 ]
    [[ "$output" == *"unavailable:"* ]]
    [[ "$output" == *"big.bin"* ]]
    # The applicable action (file.txt) is still restored despite the
    # unreachable remote -- no partial abort, no other side effects.
    [ "$(cat file.txt)" = "hi" ]
    [ ! -f big.bin ]

    reenable_remote
    rm -rf "$CLONE2"
}

@test "restore: error message names the path, never a bare hash" {
    printf 'x%.0s' {1..100} > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local CLONE2
    CLONE2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 "$CLONE2"
    [ "$status" -eq 0 ]

    cd "$CLONE2"
    rm big.bin.omemfs-stub

    run "$OMEMFS" restore --no-remote
    [ "$status" -ne 0 ]
    ! echo "$output" | grep -q "object not found"
    [[ "$output" == *"big.bin"* ]]

    rm -rf "$CLONE2"
}

@test "restore: --dry-run performs no remote I/O and reports fetch-needed paths" {
    printf 'x%.0s' {1..100} > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local CLONE2
    CLONE2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 "$CLONE2"
    [ "$status" -eq 0 ]

    cd "$CLONE2"
    rm big.bin.omemfs-stub

    # The remote is gone entirely; --dry-run must still succeed, proving it
    # made no remote access while still reporting what a real restore would
    # need to fetch.
    disable_remote

    run "$OMEMFS" restore --dry-run
    [ "$status" -eq 0 ]
    [[ "$output" == *"would fetch:"* ]]
    [[ "$output" == *"big.bin"* ]]
    [ ! -f big.bin ]

    reenable_remote
    rm -rf "$CLONE2"
}

@test "restore: a misconfigured remote's real error reaches the user when a fetch is actually needed" {
    # Distinct from "a fully-cached restore never resolves the remote, even a
    # misconfigured one" above: that test never needs a fetch at all, so it
    # only proves Phase 2's early skip. This test creates a genuine cache
    # miss (a stub deleted without ever being expanded, `Plan::pending`
    # non-empty) so Phase 2 actually calls `repo.pack_reader("does-not-
    # exist", ...)`, which previously degraded to a bare "unavailable:" with
    # no hint that `--remote` was misspelled.
    printf 'x%.0s' {1..100} > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local CLONE2
    CLONE2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 "$CLONE2"
    [ "$status" -eq 0 ]
    [ -f "$CLONE2/big.bin.omemfs-stub" ]

    cd "$CLONE2"
    rm big.bin.omemfs-stub

    run "$OMEMFS" restore --remote does-not-exist
    [ "$status" -ne 0 ]
    [[ "$output" == *"unavailable:"* ]]
    [[ "$output" == *"big.bin"* ]]
    [[ "$output" == *"not configured"* ]]

    rm -rf "$CLONE2"
}

@test "restore: heals multiple sibling missing clone-root tree objects and restores their contents" {
    # Exercises the Phase 2 heal loop with TWO simultaneous
    # `Plan::unresolved_trees` entries (not just one), covering the
    # parallel-heal batch rather than a single hash.
    mkdir -p sub_a sub_b
    echo "a content" > sub_a/a.txt
    echo "b content" > sub_b/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls -r --full-hash
    [ "$status" -eq 0 ]
    local a_hash b_hash
    a_hash=$(echo "$output" | grep 'sub_a/$' | grep -oE '[0-9a-f]{64}')
    b_hash=$(echo "$output" | grep 'sub_b/$' | grep -oE '[0-9a-f]{64}')
    [ -n "$a_hash" ]
    [ -n "$b_hash" ]

    # Drop both directories' clone-root tree objects from the local cache
    # only (they remain on the remote). The local cache uses an
    # adaptive-depth shard layout unlike the remote's fixed sharding, so
    # locate each by hash SUFFIX (same pattern as tests/ls.bats's
    # "self-heals a local cache miss" test).
    local a_obj b_obj
    a_obj=$(find .omemfs/objects -type f -name "*${a_hash: -40}" | head -1)
    b_obj=$(find .omemfs/objects -type f -name "*${b_hash: -40}" | head -1)
    [ -n "$a_obj" ]
    [ -n "$b_obj" ]
    rm -f "$a_obj" "$b_obj"

    # Modify both files so restore has real content to restore once each
    # subtree is healed.
    echo "a changed" > sub_a/a.txt
    echo "b changed" > sub_b/b.txt

    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ "$(cat sub_a/a.txt)" = "a content" ]
    [ "$(cat sub_b/b.txt)" = "b content" ]
}

@test "restore: stub reconcile still works when the remote is unreachable" {
    # Regression guard for the stub-under-restore tests above: stub-only
    # paths must never even attempt remote access, so they keep working with
    # the remote entirely gone.
    mkdir -p bigdir
    printf 'x%.0s' {1..100} > bigdir/big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local CLONE2
    CLONE2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 "$CLONE2"
    [ "$status" -eq 0 ]
    [ -f "$CLONE2/bigdir/.omemfs-stub" ]

    disable_remote

    cd "$CLONE2"
    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ -f bigdir/.omemfs-stub ]
    [ ! -f bigdir/big.bin ]

    reenable_remote
    rm -rf "$CLONE2"
}
