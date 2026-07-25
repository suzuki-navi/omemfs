#!/usr/bin/env bats
# Tests for `omemfs push`

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

@test "push: uploads files and updates INDEX_ROOT" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    [ -f "$REMOTE_DIR/INDEX_ROOT" ]
}

@test "push: clone_root equals REMOTE_ROOT after push" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    local clone_root remote_root
    clone_root="$(get_clone_root)"
    remote_root="$(get_remote_root)"
    [ "$clone_root" = "$remote_root" ]
}

@test "push: nothing to push when working tree matches clone root" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}

@test "push: objects are stored in local cache" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # At least one object file must exist in .omemfs/objects/
    local count
    count="$(find .omemfs/objects -type f | wc -l)"
    [ "$count" -gt 0 ]
}

@test "push: path-scoped push uploads only the specified subtree" {
    mkdir -p src docs
    echo "source" > src/main.rs
    echo "readme" > docs/README.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "updated source" > src/main.rs
    run "$OMEMFS" push src
    [ "$status" -eq 0 ]
}

@test "push: path-scoped push leaves other paths unchanged in remote" {
    mkdir -p src docs
    echo "source" > src/main.rs
    echo "readme" > docs/README.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Only update src/main.rs; docs/README.md must remain at old content in remote.
    echo "new source" > src/main.rs
    run "$OMEMFS" push src
    [ "$status" -eq 0 ]

    # Clone the remote into a fresh directory and verify both files exist.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ "$(cat verify/src/main.rs)" = "new source" ]
    [ "$(cat verify/docs/README.md)" = "readme" ]
}

@test "push: identical content under multiple paths is synced correctly" {
    # Same blob content in several directories exercises the BFS visited-set
    # dedup path: the duplicate hash must still be uploaded once and clone
    # correctly.
    mkdir -p a b c
    echo "shared content" > a/dup.txt
    echo "shared content" > b/dup.txt
    echo "shared content" > c/dup.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ "$(cat verify/a/dup.txt)" = "shared content" ]
    [ "$(cat verify/b/dup.txt)" = "shared content" ]
    [ "$(cat verify/c/dup.txt)" = "shared content" ]
}

@test "push: mtime stability — unchanged content keeps clone root mtime on push" {
    echo "stable content" > stable.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Record the mtime stored in clone root after first push.
    local first_root
    first_root="$(get_clone_root)"

    # Touch the file to change its filesystem mtime without changing content.
    sleep 4  # exceed RACY_THRESHOLD_SECS so it is not racy
    touch stable.txt

    # Push again; the file content is the same so the tree hash should not change
    # (mtime stability reuses the clone root mtime, not the new filesystem mtime).
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}

@test "push: mtime stability — modified content produces a new tree hash" {
    echo "original" > content.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local first_root
    first_root="$(get_clone_root)"

    sleep 4  # exceed RACY_THRESHOLD_SECS
    echo "modified" > content.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local second_root
    second_root="$(get_clone_root)"
    [ "$first_root" != "$second_root" ]
}

@test "push: path-scoped push updates clone_root for the scoped path only" {
    mkdir -p a b
    echo "afile" > a/file.txt
    echo "bfile" > b/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "afile updated" > a/file.txt
    run "$OMEMFS" push a
    [ "$status" -eq 0 ]

    # Second push of the same scoped path should report nothing to push.
    run "$OMEMFS" push a
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}

@test "push: aborts when conflict helper files exist" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Simulate leftover conflict helper files from a previous pull.
    echo "base content"   > file.txt.omemfs-conflict-base
    echo "local content"  > file.txt.omemfs-conflict-local
    echo "remote content" > file.txt.omemfs-conflict-remote

    echo "edited" > file.txt
    run "$OMEMFS" push
    [ "$status" -ne 0 ]
    [[ "$output" == *"conflict"* ]] || [[ "$output" == *"Conflict"* ]]
}

@test "push: path-scoped delete removes file from remote" {
    echo "hello" > a.txt
    echo "world" > b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    rm a.txt
    run "$OMEMFS" push a.txt
    [ "$status" -eq 0 ]
    [[ "$output" == *"Deleted"* ]]

    # Clone and verify that a.txt is gone but b.txt remains.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ ! -f "verify/a.txt" ]
    [ "$(cat verify/b.txt)" = "world" ]
}

@test "push: path-scoped delete of non-synced file is a no-op with note" {
    echo "hello" > a.txt
    # Never pushed, so the path is already absent on the remote.
    rm a.txt
    run "$OMEMFS" push a.txt
    [ "$status" -eq 0 ]
    [[ "$output" == *"already absent on remote"* ]]
}

@test "push: path-scoped delete updates clone_root" {
    echo "hello" > a.txt
    echo "world" > b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    rm a.txt
    run "$OMEMFS" push a.txt
    [ "$status" -eq 0 ]

    # Second delete push: a.txt is now absent on remote → no-op with note.
    run "$OMEMFS" push a.txt
    [ "$status" -eq 0 ]
    [[ "$output" == *"already absent on remote"* ]]

    # b.txt must still be present on the remote (delete did not wipe the tree).
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify_del
    [ "$status" -eq 0 ]
    [ -f "verify_del/b.txt" ]
    [ ! -f "verify_del/a.txt" ]
}

@test "push: conflict helper files are not included in pushed tree" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # After resolving (removing helper files), push should succeed.
    echo "base content"   > file.txt.omemfs-conflict-base
    echo "local content"  > file.txt.omemfs-conflict-local
    echo "remote content" > file.txt.omemfs-conflict-remote
    # Resolve by removing helper files.
    rm file.txt.omemfs-conflict-base file.txt.omemfs-conflict-local file.txt.omemfs-conflict-remote

    echo "resolved" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Clone and verify that helper files are NOT in the remote.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ ! -f "verify/file.txt.omemfs-conflict-base" ]
    [ ! -f "verify/file.txt.omemfs-conflict-local" ]
    [ ! -f "verify/file.txt.omemfs-conflict-remote" ]
}

# ---------------------------------------------------------------------------
# Scoped push: reject system paths
# ---------------------------------------------------------------------------

@test "push: conflict helpers inside an ignored directory do not block push" {
    # Conflict helpers underneath a path matched by .omemfs-filter are never
    # scanned; they must not cause push to fail.
    cat > .omemfs-filter <<'EOF'
[ignore]
ignored_dir/
EOF
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    mkdir -p ignored_dir
    echo "base"   > ignored_dir/x.txt.omemfs-conflict-base
    echo "local"  > ignored_dir/x.txt.omemfs-conflict-local
    echo "remote" > ignored_dir/x.txt.omemfs-conflict-remote

    echo "edited" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
}

@test "push: scoped push still blocks on a conflict helper inside the scoped path" {
    mkdir -p keep
    echo "hello" > keep/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "base"   > keep/a.txt.omemfs-conflict-base
    echo "local"  > keep/a.txt.omemfs-conflict-local
    echo "remote" > keep/a.txt.omemfs-conflict-remote

    echo "edited" > keep/a.txt
    run "$OMEMFS" push keep
    [ "$status" -ne 0 ]
    [[ "$output" == *"conflict"* ]] || [[ "$output" == *"Conflict"* ]]
}

@test "push: scoped push ignores conflict helpers outside the scoped path" {
    # A conflict helper anywhere outside the pushed subtree must not block a
    # scoped push: detection is a side effect of scanning only the specified
    # path, exactly like full push (design/04_cli_spec.md "path-scoped push").
    mkdir -p keep elsewhere
    echo "hello" > keep/a.txt
    echo "hello" > elsewhere/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "base"   > elsewhere/b.txt.omemfs-conflict-base
    echo "local"  > elsewhere/b.txt.omemfs-conflict-local
    echo "remote" > elsewhere/b.txt.omemfs-conflict-remote

    echo "edited" > keep/a.txt
    run "$OMEMFS" push keep
    [ "$status" -eq 0 ]
}

@test "push: scoped push ignores conflict helpers under an ignored directory" {
    # Same guarantee as the full-push case above, but exercised via a scoped
    # push so it also covers push_scoped's own scan/filter handling rather
    # than only push_full's.
    cat > .omemfs-filter <<'EOF'
[ignore]
ignored_dir/
EOF
    mkdir -p keep ignored_dir
    echo "hello" > keep/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "base"   > ignored_dir/x.txt.omemfs-conflict-base
    echo "local"  > ignored_dir/x.txt.omemfs-conflict-local
    echo "remote" > ignored_dir/x.txt.omemfs-conflict-remote

    echo "edited" > keep/a.txt
    run "$OMEMFS" push keep
    [ "$status" -eq 0 ]
}

@test "push: scoped push survives a symlink loop outside the scoped path" {
    # Regression test: a self-referential symlink anywhere in the working
    # tree (outside the pushed subtree) must not make a scoped push fail with
    # an I/O error. Conflict-helper detection must only ever visit the
    # specified paths' subtrees, never walk the whole work_dir following
    # symlinks (see design/04_cli_spec.md "path-scoped push").
    mkdir -p keep loop_dir
    echo "hello" > keep/a.txt
    ln -s . loop_dir/self
    run "$OMEMFS" push keep
    [ "$status" -eq 0 ]
}

@test "push: scoped push of .omemfs is rejected" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" push .omemfs
    [ "$status" -ne 0 ]
    [[ "$output" == *"system path"* ]]
}

@test "push: scoped push of a file inside .omemfs is rejected" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" push .omemfs/config
    [ "$status" -ne 0 ]
    [[ "$output" == *"system path"* ]]
}

# ---------------------------------------------------------------------------
# Scoped push: reject ignored paths
# ---------------------------------------------------------------------------

@test "push: scoped push of a directly-ignored file is rejected" {
    cat > .omemfs-filter <<'EOF'
[ignore]
secret.txt
EOF
    echo "tracked" > tracked.txt
    echo "secret" > secret.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" push secret.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"ignored"* ]]
}

@test "push: scoped push of a directly-ignored directory is rejected" {
    cat > .omemfs-filter <<'EOF'
[ignore]
build/
EOF
    mkdir build
    echo "artifact" > build/out.bin
    echo "source" > main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" push build
    [ "$status" -ne 0 ]
    [[ "$output" == *"ignored"* ]]
}

@test "push: scoped push of a file inside an ignored directory is rejected" {
    cat > .omemfs-filter <<'EOF'
[ignore]
build/
EOF
    mkdir -p build/sub
    echo "artifact" > build/sub/out.bin
    echo "source" > main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" push build/sub/out.bin
    [ "$status" -ne 0 ]
    [[ "$output" == *"ignored"* ]]
}

@test "push: scoped push of parent of an ignored directory is allowed" {
    cat > .omemfs-filter <<'EOF'
[ignore]
src/build/
EOF
    mkdir -p src/build
    echo "artifact" > src/build/out.bin
    echo "source" > src/main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # `src` itself is not ignored; the push should succeed and exclude src/build/
    run "$OMEMFS" push src
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ -f verify/src/main.rs ]
    [ ! -d verify/src/build ]
}

@test "push: scoped push respects root filter when scanning subdirectory" {
    cat > .omemfs-filter <<'EOF'
[ignore]
src/generated/
EOF
    mkdir -p src/generated src/lib
    echo "generated" > src/generated/auto.rs
    echo "lib code" > src/lib/util.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Modify lib and push only src/ — generated/ must still be excluded.
    echo "lib code updated" > src/lib/util.rs
    run "$OMEMFS" push src
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ -f verify/src/lib/util.rs ]
    [ ! -d verify/src/generated ]
}

# ---------------------------------------------------------------------------
# Backup push uses the pack layer (INDEX_ROOT), not REMOTE_ROOT
# ---------------------------------------------------------------------------

@test "push --with-backup: backup remote gets INDEX_ROOT and is clonable" {
    # Configure a second local remote as 'backup' by editing config directly
    # (the interactive 'config add-backup' needs a TTY).
    local BACKUP_DIR
    BACKUP_DIR="$(mktemp -d)"
    python - "$PWD/.omemfs/config" "$BACKUP_DIR" <<'PY'
import json, sys
cfg_path, backup_path = sys.argv[1], sys.argv[2]
with open(cfg_path) as f:
    cfg = json.load(f)
cfg["remotes"]["backup"] = {"type": "local", "path": backup_path}
with open(cfg_path, "w") as f:
    json.dump(cfg, f, indent=2)
PY

    echo "backed up" > file.txt
    run "$OMEMFS" push --with-backup
    [ "$status" -eq 0 ]

    # Backup remote must have an INDEX_ROOT and must NOT have a REMOTE_ROOT file.
    [ -f "$BACKUP_DIR/INDEX_ROOT" ]
    [ ! -f "$BACKUP_DIR/REMOTE_ROOT" ]

    # Origin must also not have a REMOTE_ROOT file (INDEX_ROOT only).
    [ ! -f "$REMOTE_DIR/REMOTE_ROOT" ]

    # A fresh clone from the backup remote must recover the file.
    run "$OMEMFS" clone --existing --url "$BACKUP_DIR" from_backup
    [ "$status" -eq 0 ]
    [ -f from_backup/file.txt ]
    [ "$(cat from_backup/file.txt)" = "backed up" ]

    rm -rf "$BACKUP_DIR"
}

@test "push: second push does not re-upload already-present objects" {
    # Regression guard for the "empty Bloom filter every push" bug. On the
    # first push, the Bloom filter / remote index reflects nothing, so all
    # objects are uploaded. A second push that only ADDS a new file must route
    # (upload) just the new blob plus the new tree object(s) — the unchanged
    # blobs and their subtrees must NOT be re-routed through the pack writer.
    #
    # Object routing is observable via the L6 "route ..." lines in the per-push
    # log file (.omemfs/logs/*-push.log). Counting them before and after gives
    # the upload count for that push.
    mkdir -p a b
    echo "alpha" > a/one.txt
    echo "beta"  > b/two.txt
    echo "gamma" > c.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local first_routes
    first_routes="$(cat .omemfs/logs/*-push.log | grep -c 'route ' || true)"
    # The first push uploads several objects (3 blobs + tree objects).
    [ "$first_routes" -ge 4 ]

    # Clear logs so the next push's routing is counted in isolation (log file
    # names have one-second resolution and would otherwise collide).
    rm -f .omemfs/logs/*-push.log

    # Second push: add a single new small file; everything else is unchanged.
    echo "delta" > new.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local second_routes
    second_routes="$(cat .omemfs/logs/*-push.log | grep -c 'route ' || true)"

    # The second push must route only the new blob plus the rewritten tree
    # objects along its path (root, and possibly nothing else since new.txt is
    # at the repo root). It must be far fewer than the first push and must not
    # re-route the unchanged a/one.txt, b/two.txt, c.txt blobs.
    [ "$second_routes" -ge 1 ]
    [ "$second_routes" -lt "$first_routes" ]
    [ "$second_routes" -le 3 ]
}

@test "push: scoped push preserves STAT_CACHE entries for out-of-scope files" {
    # Scope-limited STAT_CACHE load (design/07): a scoped push must scan and
    # write back only the in-scope slice, but the writeback merge must preserve
    # out-of-scope entries so a later full push still hits the cache for them.
    mkdir dirA dirB
    echo "a" > dirA/a.txt
    echo "b" > dirB/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Both entries must be present after the initial full push.
    grep -aq "dirA/a.txt" .omemfs/STAT_CACHE
    grep -aq "dirB/b.txt" .omemfs/STAT_CACHE

    # Modify a file in dirA and push only dirA. Wait past the racy window so the
    # re-hashed entry is recorded as safe.
    echo "a2" > dirA/a.txt
    sleep 3
    run "$OMEMFS" push dirA
    [ "$status" -eq 0 ]

    # The out-of-scope dirB/b.txt entry must survive the scoped writeback merge,
    # and the in-scope dirA/a.txt entry must still be present.
    grep -aq "dirB/b.txt" .omemfs/STAT_CACHE
    grep -aq "dirA/a.txt" .omemfs/STAT_CACHE
}

# ---------------------------------------------------------------------------
# push: delta index files are cached locally so remote reads stay flat
# ---------------------------------------------------------------------------
# After several delta index files have accumulated on the remote, repeated
# small pushes must NOT re-fetch those immutable index files on every push.
# With PackWriter's local-cache optimisation, the per-push remote "reads"
# count should stay roughly constant (not grow with the number of deltas).
# ---------------------------------------------------------------------------

@test "push: per-push remote reads stay flat after delta index files accumulate" {
    local jsonl=".omemfs/io_stats.jsonl"

    # Seed: do 5 small pushes to accumulate several delta index files on the remote.
    for i in $(seq 1 5); do
        echo "seed $i" > "seed_$i.txt"
        run "$OMEMFS" push "seed_$i.txt"
        [ "$status" -eq 0 ]
    done

    # Clear the io_stats log so we only measure the next 3 pushes.
    rm -f "$jsonl"

    # Do 3 more small pushes, each modifying a single tiny file.
    for i in $(seq 1 3); do
        echo "round2 $i" > "round2_$i.txt"
        run "$OMEMFS" push "round2_$i.txt"
        [ "$status" -eq 0 ]
    done

    # The log must have exactly 3 push records.
    [ -f "$jsonl" ]
    local push_count
    push_count=$(grep -c '"cmd":"push"' "$jsonl" || true)
    [ "$push_count" -eq 3 ]

    # Extract the "reads" value from each of the 3 push records.
    local reads1 reads2 reads3
    reads1=$(grep '"cmd":"push"' "$jsonl" | sed -n '1p' | grep -oE '"reads":[0-9]+' | grep -oE '[0-9]+$')
    reads2=$(grep '"cmd":"push"' "$jsonl" | sed -n '2p' | grep -oE '"reads":[0-9]+' | grep -oE '[0-9]+$')
    reads3=$(grep '"cmd":"push"' "$jsonl" | sed -n '3p' | grep -oE '"reads":[0-9]+' | grep -oE '[0-9]+$')

    [ -n "$reads1" ]
    [ -n "$reads2" ]
    [ -n "$reads3" ]

    # The reads count must NOT grow monotonically: push 3 must not exceed push 1
    # by more than 2 (a small tolerance for non-index reads like the bloom filter).
    # Before the fix, reads grew linearly with the number of delta index files
    # (e.g. 24 → 79 reads over 6 pushes). After the fix, delta index files are
    # served from the local cache after the first fetch, so reads stay flat.
    local max_growth=2
    [ "$reads3" -le "$((reads1 + max_growth))" ]
}

@test "push: fails (does not silently omit) when a subdirectory is unreadable" {
    # refactor-instructions.md F6: a permission error while listing a
    # subdirectory must fail the scan/push, not be silently treated as an
    # empty directory (which would push a smaller tree and, from the
    # remote's perspective, look identical to the user having deleted the
    # unreadable files).
    if [ "$(id -u)" -eq 0 ]; then
        skip "test requires a non-root user (root bypasses directory permissions)"
    fi

    echo "visible" > visible.txt
    mkdir -p secret
    echo "hidden" > secret/inside.txt
    chmod 000 secret

    run "$OMEMFS" push
    chmod 700 secret  # restore before any cleanup/rm -rf

    [ "$status" -ne 0 ]
    # The remote must not have been written to (no partial/smaller-tree push).
    [ ! -f "$REMOTE_DIR/INDEX_ROOT" ]
}

@test "push: file deleted after listing is preserved while stable changes are pushed" {
    echo "active-old" > active.txt
    echo "stable-old" > stable.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "stable-new" > stable.txt
    (sleep 0.05; rm active.txt) &
    local remover=$!
    run env OMEMFS_TEST_SCAN_AFTER_LIST_DELAY_MS=200 "$OMEMFS" push
    wait "$remover"

    [ "$status" -eq 0 ]
    [[ "$output" == *"actively changing path was not updated"* ]]
    [[ "$output" == *"active.txt"* ]]

    local dest
    dest="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$dest/verify"
    [ "$status" -eq 0 ]
    (
        cd "$dest/verify"
        "$OMEMFS" pull >/dev/null
        [ "$(cat active.txt)" = "active-old" ]
        [ "$(cat stable.txt)" = "stable-new" ]
    )
    rm -rf "$dest"
}

@test "push: continuously edited file is preserved while stable changes are pushed" {
    echo "active-old" > active.txt
    echo "stable-old" > stable.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "stable-new" > stable.txt
    local stop_file="$TEST_DIR/stop-writer"
    (
        i=0
        while [ ! -e "$stop_file" ]; do
            printf 'active-write-%08d-%08d\n' "$i" "$RANDOM" > active.txt
            i=$((i + 1))
            sleep 0.01
        done
    ) &
    local writer=$!
    sleep 0.05

    run env OMEMFS_TEST_SNAPSHOT_POST_READ_DELAY_MS=150 "$OMEMFS" push
    touch "$stop_file"
    wait "$writer"

    [ "$status" -eq 0 ]
    [[ "$output" == *"actively changing path was not updated"* ]]
    [[ "$output" == *"active.txt"* ]]

    local dest
    dest="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$dest/verify"
    [ "$status" -eq 0 ]
    (
        cd "$dest/verify"
        "$OMEMFS" pull >/dev/null
        [ "$(cat active.txt)" = "active-old" ]
        [ "$(cat stable.txt)" = "stable-new" ]
    )
    rm -rf "$dest"
}

@test "push: directory removed after parent listing preserves previous subtree" {
    mkdir -p active-dir
    echo "nested-old" > active-dir/nested.txt
    echo "stable-old" > stable.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "stable-new" > stable.txt
    (sleep 0.05; rm -rf active-dir) &
    local remover=$!
    run env OMEMFS_TEST_SCAN_AFTER_LIST_DELAY_MS=200 "$OMEMFS" push
    wait "$remover"

    [ "$status" -eq 0 ]
    [[ "$output" == *"active-dir"* ]]

    local dest
    dest="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$dest/verify"
    [ "$status" -eq 0 ]
    (
        cd "$dest/verify"
        "$OMEMFS" pull >/dev/null
        [ "$(cat active-dir/nested.txt)" = "nested-old" ]
        [ "$(cat stable.txt)" = "stable-new" ]
    )
    rm -rf "$dest"
}

@test "push: stable full-tree deletion is still applied" {
    echo "delete-me" > deleted.txt
    echo "keep-me" > kept.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    rm deleted.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local dest
    dest="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$dest/verify"
    [ "$status" -eq 0 ]
    [ ! -e "$dest/verify/deleted.txt" ]
    [ "$(cat "$dest/verify/kept.txt")" = "keep-me" ]
    rm -rf "$dest"
}

@test "push: path absent at first listing but reappearing is not deleted" {
    echo "remote-old" > active.txt
    echo "stable-old" > stable.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    rm active.txt
    echo "stable-new" > stable.txt
    (sleep 0.05; echo "replacement" > active.txt) &
    local replacer=$!
    run env OMEMFS_TEST_SCAN_AFTER_LIST_DELAY_MS=200 "$OMEMFS" push
    wait "$replacer"

    [ "$status" -eq 0 ]
    [[ "$output" == *"active.txt"* ]]

    local dest
    dest="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$dest/verify"
    [ "$status" -eq 0 ]
    [ "$(cat "$dest/verify/active.txt")" = "remote-old" ]
    [ "$(cat "$dest/verify/stable.txt")" = "stable-new" ]
    rm -rf "$dest"
}

# ---------------------------------------------------------------------------
# refactor-instructions.md Phase 8 (E7) step 3b: behaviour-pinning tests for
# push_scoped_multi / push_scoped, added before the push-side path
# consolidation. push_scoped_multi's per-path loop never checked whether a
# stub or a regular file/dir already matched clone_root before splicing it,
# unlike push_scoped's single-path "Nothing to push" short-circuit -- a gap
# invisible today because no existing test drives push_scoped_multi with a
# path that has not changed. The multi-path tests below are target/pinning
# tests for the fix that closes that gap as part of the consolidation.
# ---------------------------------------------------------------------------

@test "push: path-scoped push of an unchanged stub reports nothing to push" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    [ "$status" -eq 0 ]
    cd repo
    echo "keep-me" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" stub file.txt
    [ "$status" -eq 0 ]

    # Scoped push of the unchanged stub must be a no-op, not a real write.
    run "$OMEMFS" push file.txt
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}

@test "push: multi-path push of two unchanged paths reports nothing to push" {
    # refactor-instructions.md Phase 8 (E7): push_scoped_multi's per-path loop
    # must skip a path whose freshly scanned hash already matches clone_root,
    # the same way push_scoped's single-path check does -- otherwise a
    # multi-path push always performs a full write cycle even when nothing
    # changed. Before the fix this test is expected to FAIL (status 0 but no
    # "nothing to push" -- a real, empty-content CAS write happens instead).
    mkdir -p a b
    echo "afile" > a/file.txt
    echo "bfile" > b/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Neither a/ nor b/ changed since the full push above.
    run "$OMEMFS" push a b
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}

@test "push: multi-path push skips the unchanged path and pushes only the changed one" {
    mkdir -p a b
    echo "afile" > a/file.txt
    echo "bfile" > b/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "afile updated" > a/file.txt
    run "$OMEMFS" push a b
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify_multi
    [ "$status" -eq 0 ]
    [ "$(cat verify_multi/a/file.txt)" = "afile updated" ]
    [ "$(cat verify_multi/b/file.txt)" = "bfile" ]
}

# ---------------------------------------------------------------------------
# push: scoped flatten (design/03 "Path-scoped push") -- push_scoped must stop
# walking the whole clone root, and its scope-limited STAT_CACHE handling must
# not disturb out-of-scope entries.
# ---------------------------------------------------------------------------

@test "push: scoped push leaves out-of-scope STAT_CACHE usable (push b reports nothing to push)" {
    mkdir -p a b
    echo "a1" > a/file.txt
    echo "b1" > b/file.txt
    # Backdate both files well outside the racy window so STAT_CACHE hits are
    # deterministic (RACY_THRESHOLD_SECS, design/07) -- avoids a sleep-based
    # test (pattern from tests/chunk.bats).
    touch -d "2020-01-01 00:00:00" a/file.txt b/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Modify only a/file.txt and push only a. This must not disturb b's
    # STAT_CACHE entry (scoped flatten + scoped STAT_CACHE writeback merge).
    echo "a2" > a/file.txt
    touch -d "2020-01-01 00:00:00" a/file.txt
    run "$OMEMFS" push a
    [ "$status" -eq 0 ]

    # b's STAT_CACHE entry must have survived the scoped writeback merge: a
    # push of b immediately afterward must be a pure cache hit (b/file.txt did
    # not change), observable as "Nothing to push."
    run "$OMEMFS" push b
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}

@test "push: scoped push of a new path absent from clone root succeeds (empty-map branch)" {
    # c/ does not exist anywhere in clone_root yet, so flatten_tree_entries_scoped
    # must take the "path absent" branch (empty map) rather than erroring.
    echo "root file" > root.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    mkdir -p c
    echo "new content" > c/file.txt
    run "$OMEMFS" push c
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify_new_scope
    [ "$status" -eq 0 ]
    [ "$(cat verify_new_scope/c/file.txt)" = "new content" ]

    # A subsequent scoped push of the same, now-unchanged, path must be a
    # pure no-op.
    run "$OMEMFS" push c
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}
