#!/usr/bin/env bats
# Tests for the cost-oriented `omemfs stats` output and pack instrumentation:
#   - pack records real (non-zero) I/O plus a pack_detail object in io_stats.jsonl
#   - stats "Remote storage" section (counts/bytes) for a local remote
#   - stats "Pack effectiveness" section appears after pack, omitted before
#   - stats "Remote storage" orphans line is present

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# Push several small-ish blobs so pack has candidate pack files to consolidate.
push_some_blobs() {
    local n="${1:-6}"
    for i in $(seq 1 "$n"); do
        head -c 200000 /dev/urandom | base64 > "blob$i.dat"
    done
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
}

# ---------------------------------------------------------------------------
# Test 1: pack writes a real-instrumented record with a pack_detail object.
# ---------------------------------------------------------------------------

@test "stats_cost: pack record has non-zero I/O and a pack_detail object" {
    push_some_blobs 6

    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    local jsonl=".omemfs/io_stats.jsonl"
    [ -f "$jsonl" ]

    # The pack record must exist.
    local rec
    rec=$(grep '"cmd":"pack"' "$jsonl" | tail -1)
    [ -n "$rec" ]

    # It must carry a pack_detail object.
    [[ "$rec" == *'"pack_detail"'* ]]

    # writes and reads must NOT both be zero (real instrumentation, not the old
    # zero-count placeholder).
    local writes reads
    writes=$(echo "$rec" | grep -oE '"writes":[0-9]+' | grep -oE '[0-9]+$')
    reads=$(echo "$rec" | grep -oE '"reads":[0-9]+' | grep -oE '[0-9]+$')
    [ -n "$writes" ]
    [ -n "$reads" ]
    [ $(( writes + reads )) -gt 0 ]
}

# ---------------------------------------------------------------------------
# Test 1b: pack-scheduling data — duration_ms on every record, deltas_after
# only on push, orphaned_bytes on pack_detail.
# ---------------------------------------------------------------------------

@test "stats_cost: push records deltas_after growing with each push; pack resets it away" {
    echo "one" > file1.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "two" > file2.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local jsonl=".omemfs/io_stats.jsonl"
    local first_deltas second_deltas
    first_deltas=$(grep '"cmd":"push"' "$jsonl" | sed -n '1p' | grep -oE '"deltas_after":[0-9]+' | grep -oE '[0-9]+$')
    second_deltas=$(grep '"cmd":"push"' "$jsonl" | sed -n '2p' | grep -oE '"deltas_after":[0-9]+' | grep -oE '[0-9]+$')
    [ -n "$first_deltas" ]
    [ -n "$second_deltas" ]
    [ "$second_deltas" -gt "$first_deltas" ]

    # Non-push records (e.g. a subsequent pack) must NOT carry deltas_after.
    push_some_blobs 6
    run "$OMEMFS" pack
    [ "$status" -eq 0 ]
    local pack_rec
    pack_rec=$(grep '"cmd":"pack"' "$jsonl" | tail -1)
    [[ "$pack_rec" != *'"deltas_after"'* ]]
}

@test "stats_cost: every record carries duration_ms" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local jsonl=".omemfs/io_stats.jsonl"
    local rec duration
    rec=$(grep '"cmd":"push"' "$jsonl" | tail -1)
    duration=$(echo "$rec" | grep -oE '"duration_ms":[0-9]+' | grep -oE '[0-9]+$')
    [ -n "$duration" ]
    [ "$duration" -ge 0 ]
}

@test "stats_cost: pack_detail carries orphaned_bytes reflecting the superseded hot index" {
    push_some_blobs 6

    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    local jsonl=".omemfs/io_stats.jsonl"
    local rec orphaned
    rec=$(grep '"cmd":"pack"' "$jsonl" | tail -1)
    [[ "$rec" == *'"orphaned_bytes"'* ]]
    orphaned=$(echo "$rec" | grep -oE '"orphaned_bytes":[0-9]+' | grep -oE '[0-9]+$')
    [ -n "$orphaned" ]

    # A second pack run (no INDEX_ROOT change in between other than this pack
    # itself having just run) still writes a NEW hot index/bloom, so the old
    # ones (from the first pack) become orphaned again: orphaned_bytes must be
    # positive on this second run too.
    run "$OMEMFS" pack
    [ "$status" -eq 0 ]
    rec=$(grep '"cmd":"pack"' "$jsonl" | tail -1)
    orphaned=$(echo "$rec" | grep -oE '"orphaned_bytes":[0-9]+' | grep -oE '[0-9]+$')
    [ -n "$orphaned" ]
    [ "$orphaned" -gt 0 ]
}

# ---------------------------------------------------------------------------
# Test 2: stats shows a Remote storage section with counts/bytes after push.
# ---------------------------------------------------------------------------

@test "stats_cost: Remote storage section appears with object/byte counts after push" {
    push_some_blobs 3

    run "$OMEMFS" stats --remote
    [ "$status" -eq 0 ]
    [[ "$output" == *"Remote storage  (origin)"* ]]
    [[ "$output" == *"pack-files"* ]]
    [[ "$output" == *"index-files"* ]]
    [[ "$output" == *"index-root"* ]]
    # The grand total line must be present.
    [[ "$output" == *"total"* ]]
}

# ---------------------------------------------------------------------------
# Test 3: Pack effectiveness shows a placeholder before pack (when I/O history
# exists) and the real metrics after pack.
# ---------------------------------------------------------------------------

@test "stats_cost: Pack effectiveness placeholder before pack, metrics after pack" {
    push_some_blobs 6

    # Before any pack run there is I/O history (the push), so the header is
    # shown with a placeholder rather than being omitted.
    run "$OMEMFS" stats
    [ "$status" -eq 0 ]
    [[ "$output" == *"Pack effectiveness"* ]]
    [[ "$output" == *"no recent consolidation recorded"* ]]

    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    # After pack: section must be present with its real fields and no placeholder.
    run "$OMEMFS" stats
    [ "$status" -eq 0 ]
    [[ "$output" == *"Pack effectiveness"* ]]
    [[ "$output" == *"delta indexes merged"* ]]
    [[ "$output" == *"hot index"* ]]
    [[ "$output" == *"bloom"* ]]
    [[ "$output" != *"no recent consolidation recorded"* ]]
}

# ---------------------------------------------------------------------------
# Test 4: Remote storage orphans line is present. Consolidation during pack
# leaves the old pack file (and superseded index/bloom) unreferenced, so an
# orphan count > 0 is expected; assert at minimum the line exists.
# ---------------------------------------------------------------------------

@test "stats_cost: Remote storage orphans line present (reclaimable via backup-reclone)" {
    push_some_blobs 6
    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    run "$OMEMFS" stats --remote
    [ "$status" -eq 0 ]
    [[ "$output" == *"orphans"* ]]
    [[ "$output" == *"reclaimable via backup-reclone"* ]]

    # Extract the orphans count and assert it is a non-negative integer.
    local orphans
    orphans=$(echo "$output" | grep 'orphans' | grep -oE '[0-9]+' | head -1)
    [ -n "$orphans" ]
    [ "$orphans" -ge 0 ]
}

# ---------------------------------------------------------------------------
# Test 5: stats --json includes remote_storage and pack_effectiveness objects.
# ---------------------------------------------------------------------------

@test "stats_cost: stats --json includes remote_storage and pack_effectiveness" {
    push_some_blobs 6
    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    run "$OMEMFS" stats --remote --json
    [ "$status" -eq 0 ]
    [[ "$output" == *'"remote_storage"'* ]]
    [[ "$output" == *'"orphans"'* ]]
    [[ "$output" == *'"pack_effectiveness"'* ]]
    [[ "$output" == *'"deltas_merged"'* ]]
    # Existing keys must remain intact.
    [[ "$output" == *'"io_history"'* ]]
    [[ "$output" == *'"io_totals"'* ]]
}

# ---------------------------------------------------------------------------
# Test 5b: Without --remote, the two remote-backed sections are omitted in
# both text and JSON, even though origin is a local remote with objects.
# This is the default offline-safe behaviour: no remote enumeration occurs.
# ---------------------------------------------------------------------------

@test "stats_cost: remote sections omitted by default (no --remote)" {
    push_some_blobs 3

    # Text: neither remote section header appears.
    run "$OMEMFS" stats
    [ "$status" -eq 0 ]
    [[ "$output" != *"Remote storage  (origin)"* ]]
    [[ "$output" != *"Remote object sizes"* ]]
    # Local sections still present.
    [[ "$output" == *"Local cache composition"* ]]
    [[ "$output" == *"Working-tree file sizes"* ]]

    # JSON: neither remote key appears.
    run "$OMEMFS" stats --json
    [ "$status" -eq 0 ]
    [[ "$output" != *'"remote_storage"'* ]]
    [[ "$output" != *'"remote_object_histogram"'* ]]
    # Local keys still present.
    [[ "$output" == *'"worktree_file_histogram"'* ]]
    [[ "$output" == *'"by_type"'* ]]
}

# ---------------------------------------------------------------------------
# Test 6: On an ENCRYPTED remote the index root lives under objects/ at its
# derived 64-hex name. Its LIST key is that full hex (reconstructed from the
# sharded path), so classify_remote must count it as index-root, NOT sweep it
# into orphans and NOT double-count it via the unencrypted metadata fallback.
# Regression guard for the index-root-known-key mismatch (file-name leaf vs
# full storage key) on encrypted remotes.
# ---------------------------------------------------------------------------

@test "stats_cost: encrypted remote counts index-root correctly (not orphan, not double-counted)" {
    # Fresh encrypted clone in a dedicated subdir against an empty remote.
    local ENC="$TEST_DIR/enc"
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" --encrypt "$ENC"
    [ "$status" -eq 0 ]

    cd "$ENC"
    head -c 200000 /dev/urandom | base64 > blob.dat
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stats --remote --json
    [ "$status" -eq 0 ]
    [[ "$output" == *'"remote_storage"'* ]]

    # Exactly one index-root object, counted in the index_root class.
    local ir_count
    ir_count=$(echo "$output" | grep -A1 '"index_root"' | grep -oE '"count": [0-9]+' | grep -oE '[0-9]+$' | head -1)
    [ "$ir_count" = "1" ]

    # The index root must NOT have leaked into orphans. With a single push (no
    # pack consolidation), there is nothing reclaimable, so orphans must be 0.
    local orphan_count
    orphan_count=$(echo "$output" | grep -A1 '"orphans"' | grep -oE '"count": [0-9]+' | grep -oE '[0-9]+$' | head -1)
    [ "$orphan_count" = "0" ]
}
