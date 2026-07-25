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
