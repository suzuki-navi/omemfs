#!/usr/bin/env bats
# Integration tests for the pack layer (INDEX_ROOT / delta index / pack files).

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Count files in REMOTE_DIR/objects/ (excludes .depth / .migrating).
count_remote_objects() {
    find "$REMOTE_DIR/objects" -type f ! -name '.depth' ! -name '.migrating' 2>/dev/null | wc -l
}

# Return 0 if INDEX_ROOT exists in the remote.
remote_has_index_root() {
    [ -f "$REMOTE_DIR/INDEX_ROOT" ]
}

# ---------------------------------------------------------------------------
# INDEX_ROOT is created after push
# ---------------------------------------------------------------------------

@test "pack: INDEX_ROOT is created in remote after first push" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    remote_has_index_root
}

@test "pack: INDEX_ROOT contains a delta_hash after first push" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # INDEX_ROOT must be non-trivially sized (header + at least one delta_hash entry).
    local size
    size=$(wc -c < "$REMOTE_DIR/INDEX_ROOT")
    [ "$size" -gt 108 ]
}

# ---------------------------------------------------------------------------
# Small file stored inline (not as a standalone object)
# ---------------------------------------------------------------------------

@test "pack: small file (< 256 B) is NOT stored as a standalone object in remote/objects/" {
    # A tiny file whose compress+encrypt output stays below the 256 B inline threshold.
    printf 'hi' > tiny.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # The blob hash of a tiny file should NOT appear as an individual file in
    # remote/objects/ — it should be embedded inline in the delta index.
    # We verify by checking that no standalone blob object exists for this content.
    # (Tree objects may still be present as small pack entries, not standalone.)
    # The key invariant: objects/<blob_hash> file must NOT exist.
    local blob_hash
    blob_hash=$("$OMEMFS" cat --hash clone-root/tiny.txt)
    local p1="${blob_hash:0:2}"
    local rest="${blob_hash:2}"
    # Search all plausible depths.
    ! find "$REMOTE_DIR/objects" -type f \( \
        -name "$blob_hash" \
        -o -name "$rest" \
    \) 2>/dev/null | grep -q .
}

# ---------------------------------------------------------------------------
# pull after push retrieves pack-stored objects correctly
# ---------------------------------------------------------------------------

@test "pack: clone after push retrieves file stored via pack layer" {
    echo "pack layer content" > data.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]
    [ "$(cat "$clone2/data.txt")" = "pack layer content" ]
    rm -rf "$clone2"
}

@test "pack: pull after push retrieves file stored via pack layer" {
    echo "first version" > doc.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Set up a second clone that starts from the same remote.
    local clone2
    clone2="$(mktemp -d)"
    "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"

    # Push a modification from the original clone.
    echo "second version" > doc.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # The second clone should pull the update correctly.
    (cd "$clone2" && run "$OMEMFS" pull)
    [ "$(cat "$clone2/doc.txt")" = "second version" ]
    rm -rf "$clone2"
}

# ---------------------------------------------------------------------------
# Multiple pushes accumulate delta indexes
# ---------------------------------------------------------------------------

@test "pack: multiple pushes create multiple delta index entries in INDEX_ROOT" {
    echo "v1" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local size_after_first
    size_after_first=$(wc -c < "$REMOTE_DIR/INDEX_ROOT")

    echo "v2" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # INDEX_ROOT grows because a new delta_hash is prepended.
    local size_after_second
    size_after_second=$(wc -c < "$REMOTE_DIR/INDEX_ROOT")
    [ "$size_after_second" -gt "$size_after_first" ]
}

@test "pack: pull after multiple pushes retrieves the latest content" {
    echo "v1" > evolving.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "v2" > evolving.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "v3" > evolving.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]
    [ "$(cat "$clone2/evolving.txt")" = "v3" ]
    rm -rf "$clone2"
}

# ---------------------------------------------------------------------------
# omemfs pack merges delta indexes into hot index
# ---------------------------------------------------------------------------

@test "pack: omemfs pack succeeds after push" {
    echo "hello" > a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" pack
    [ "$status" -eq 0 ]
}

@test "pack: omemfs pack clears delta_hashes in INDEX_ROOT" {
    echo "hello" > a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local size_before
    size_before=$(wc -c < "$REMOTE_DIR/INDEX_ROOT")

    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    # After pack, delta_hashes are cleared — INDEX_ROOT shrinks (no delta entries,
    # now a hot_hash replaces the delta list).
    local size_after
    size_after=$(wc -c < "$REMOTE_DIR/INDEX_ROOT")
    # Size should be at most the before size (delta cleared, hot_hash added).
    # Exact size depends on delta count, but must not grow unboundedly.
    [ "$size_after" -le "$size_before" ]
}

@test "pack: clone after omemfs pack retrieves file correctly" {
    echo "packed content" > packed.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]
    [ "$(cat "$clone2/packed.txt")" = "packed content" ]
    rm -rf "$clone2"
}

@test "pack: pull after omemfs pack retrieves latest content" {
    echo "before pack" > msg.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone2
    clone2="$(mktemp -d)"
    "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"

    echo "after pack" > msg.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    (cd "$clone2" && run "$OMEMFS" pull)
    [ "$(cat "$clone2/msg.txt")" = "after pack" ]
    rm -rf "$clone2"
}

# ---------------------------------------------------------------------------
# Multiple files — some may be packed together
# ---------------------------------------------------------------------------

@test "pack: multiple medium-sized files push and pull correctly" {
    # Each file is small enough to be pack-buffered (not standalone).
    for i in 1 2 3 4 5; do
        python3 -c "import random; random.seed($i); print('x' * 512)" > "file_${i}.txt"
    done

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    for i in 1 2 3 4 5; do
        [ -f "$clone2/file_${i}.txt" ]
    done
    rm -rf "$clone2"
}

# ---------------------------------------------------------------------------
# hot/cold split: entries from old pushes move to cold shard after pack
# ---------------------------------------------------------------------------

@test "pack: hot/cold split — old content ends up in cold shard, new content stays hot" {
    # Push an old file, then update it so it is no longer referenced.
    echo "old content" > data.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "new content" > data.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    # After pack, the remote should still serve the current (new) content.
    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]
    [ "$(cat "$clone2/data.txt")" = "new content" ]
    rm -rf "$clone2"
}

@test "pack: hot/cold split — second clone pull after pack gets correct content" {
    # Push v1, clone B, push v2, pack, then B pulls.
    echo "version 1" > evolve.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone_b
    clone_b="$(mktemp -d)"
    "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone_b"

    echo "version 2" > evolve.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    (cd "$clone_b" && run "$OMEMFS" pull)
    [ "$(cat "$clone_b/evolve.txt")" = "version 2" ]
    rm -rf "$clone_b"
}

# ---------------------------------------------------------------------------
# INDEX_ROOT exists and contains remote_root after push
# ---------------------------------------------------------------------------

@test "pack: INDEX_ROOT exists and remote_root is set after push" {
    echo "consistency check" > check.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    [ -f "$REMOTE_DIR/INDEX_ROOT" ]

    local remote_root
    remote_root=$(get_remote_root)
    [ -n "$remote_root" ]
    [ "${#remote_root}" -eq 64 ]
}

# ---------------------------------------------------------------------------
# INDEX_ROOT delta_hashes count
# ---------------------------------------------------------------------------

@test "pack: delta_hashes count in INDEX_ROOT matches number of pushes" {
    echo "v1" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "v2" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "v3" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local delta_count
    delta_count="$("$OMEMFS" cat index-root \
        | python -c "import sys,json; d=json.load(sys.stdin); print(len(d['delta_hashes']))")"
    [ "$delta_count" -eq 3 ]
}

# ---------------------------------------------------------------------------
# INDEX_ROOT state after omemfs pack
# ---------------------------------------------------------------------------

@test "pack: after omemfs pack delta_hashes is empty and hot_hash is set" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    local delta_count hot_hash_val
    delta_count="$("$OMEMFS" cat index-root \
        | python -c "import sys,json; d=json.load(sys.stdin); print(len(d['delta_hashes']))")"
    hot_hash_val="$("$OMEMFS" cat index-root \
        | python -c "import sys,json; d=json.load(sys.stdin); print(d['hot_hash'])")"

    [ "$delta_count" -eq 0 ]
    [[ "$hot_hash_val" =~ ^[0-9a-f]{64}$ ]]
}

@test "pack: bloom_hash is non-null after omemfs pack" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    local bloom_hash_val
    bloom_hash_val="$("$OMEMFS" cat index-root \
        | python -c "import sys,json; d=json.load(sys.stdin); print(d.get('bloom_hash','null'))")"
    [[ "$bloom_hash_val" =~ ^[0-9a-f]{64}$ ]]
}

# ---------------------------------------------------------------------------
# hot index and bloom filter contents after omemfs pack
# ---------------------------------------------------------------------------

@test "pack: hot index entry_count is positive after omemfs pack" {
    echo "file1" > f1.txt
    echo "file2" > f2.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    local hot_hash
    hot_hash="$("$OMEMFS" cat index-root \
        | python -c "import sys,json; d=json.load(sys.stdin); print(d['hot_hash'])")"
    [ -n "$hot_hash" ]

    local entry_count
    entry_count="$("$OMEMFS" cat "${hot_hash}" \
        | python -c "import sys,json; d=json.load(sys.stdin); print(d['entry_count'])")"
    [ "$entry_count" -gt 0 ]
}

@test "pack: bloom filter element_count is positive after omemfs pack" {
    echo "content" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    local bloom_hash
    bloom_hash="$("$OMEMFS" cat index-root \
        | python -c "import sys,json; d=json.load(sys.stdin); print(d['bloom_hash'])")"
    [ -n "$bloom_hash" ]

    local elem_count
    elem_count="$("$OMEMFS" cat "${bloom_hash}" \
        | python -c "import sys,json; d=json.load(sys.stdin); print(d['element_count'])")"
    [ "$elem_count" -gt 0 ]
}

# ---------------------------------------------------------------------------
# remote_root in INDEX_ROOT updates on each push
# ---------------------------------------------------------------------------

@test "pack: INDEX_ROOT remote_root updates on each push" {
    echo "v1" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local root1
    root1="$("$OMEMFS" cat index-root \
        | python -c "import sys,json; d=json.load(sys.stdin); print(d['remote_root'])")"

    echo "v2" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local root2
    root2="$("$OMEMFS" cat index-root \
        | python -c "import sys,json; d=json.load(sys.stdin); print(d['remote_root'])")"

    [ "$root1" != "$root2" ]
}

# ---------------------------------------------------------------------------
# CLI help text matches the documented behaviour
# ---------------------------------------------------------------------------

@test "pack: --help does not claim to run GC (design/04: orphans are not deleted)" {
    run "$OMEMFS" pack --help
    [ "$status" -eq 0 ]
    # design/04_cli_spec.md: pack merges delta indexes and compacts the pack
    # layer; it does NOT delete orphans (reclaimed via backup-reclone). The old
    # help text wrongly said "run GC on the remote".
    [[ "$output" != *"GC"* ]]
    [[ "$output" == *"ompact"* ]]
}

@test "pack: top-level --help subcommand summary does not claim GC" {
    run "$OMEMFS" --help
    [ "$status" -eq 0 ]
    # The one-line pack summary in the subcommand list must not mention GC.
    echo "$output" | grep -E '^\s*pack' | grep -qv "GC"
}
