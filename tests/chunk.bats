#!/usr/bin/env bats
# Tests for the chunk stage (large-object splitting via FastCDC).

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# Generate a file of the specified size (in bytes) filled with pseudo-random data.
make_large_file() {
    local path="$1"
    local size="$2"
    head -c "$size" /dev/urandom > "$path"
}

# Count objects in a directory whose first 2 bytes match the given hex pair.
# Also scans inside pack files (ED E0 prefix) for objects with the given tag,
# by searching the raw pack body bytes for the 2-byte pattern.
count_objects_with_tag() {
    local dir="$1"
    local tag="$2"  # e.g. "edf2"
    local count=0
    while IFS= read -r -d '' f; do
        local magic
        magic=$(head -c 2 "$f" | od -An -tx1 | tr -d ' \n')
        if [ "$magic" = "$tag" ]; then
            count=$((count + 1))
        elif [ "$magic" = "ede0" ]; then
            # Pack file: scan body for tag occurrences (unencrypted repo only).
            local pack_hits
            # Convert 2-char hex tag to a 1-byte grep pattern.
            local byte1 byte2
            byte1=$(printf '%s' "$tag" | cut -c1-2)
            byte2=$(printf '%s' "$tag" | cut -c3-4)
            # Use od + awk to count occurrences of the 2-byte sequence in the pack body.
            pack_hits=$(tail -c +3 "$f" | od -An -tx1 | tr -d ' \n' | grep -o "${byte1}${byte2}" | wc -l)
            count=$((count + pack_hits))
        fi
    done < <(find "$dir" -type f ! -name '.depth' ! -name '.migrating' ! -name 'REMOTE_ROOT' ! -name 'INDEX_ROOT' -print0 2>/dev/null)
    echo "$count"
}

# ---- small file (below CDC avg threshold): stored as a single object ----

@test "chunk: small file push and pull roundtrip" {
    echo "hello chunk" > small.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone2
    clone2="$(mktemp -d)"
    "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    (cd "$clone2" && "$OMEMFS" pull)

    [ "$(cat "$clone2/small.txt")" = "hello chunk" ]
    rm -rf "$clone2"
}

@test "chunk: small file is stored as a single object (no manifest)" {
    echo "hello" > small.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # No manifest object (ED F2) should exist for a small blob.
    local found
    found=$(count_objects_with_tag "$REMOTE_DIR/objects" "edf2")
    [ "$found" -eq 0 ]
}

# ---- large file (above CDC avg threshold): split into chunks ----

@test "chunk: large file push stores manifest (ED F2) and chunk objects (ED F3) in remote" {
    # 5 MiB > avg_size (4 MiB), so FastCDC should produce at least 2 chunks.
    make_large_file big.bin $((20 * 1024 * 1024))

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Chunk objects (ED F3) must exist as standalone objects in remote/objects/
    # (each chunk is >= 1 MiB, so they are stored as standalone).
    local chunks
    chunks=$(count_objects_with_tag "$REMOTE_DIR/objects" "edf3")
    [ "$chunks" -ge 2 ]

    # The manifest (ED F2) is stored inside the pack layer (pack file or delta index).
    # Verify indirectly: a second clone must be able to retrieve the full file.
    local clone2
    clone2="$(mktemp -d)"
    "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    (cd "$clone2" && "$OMEMFS" expand --recursive)
    [ -f "$clone2/big.bin" ]
    rm -rf "$clone2"
}

@test "chunk: large file roundtrip via push/pull preserves content" {
    make_large_file big.bin $((20 * 1024 * 1024))
    local original_hash
    original_hash="$(sha256sum big.bin | cut -d' ' -f1)"

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone2
    clone2="$(mktemp -d)"
    "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    # Large files are stubbed on clone; expand to materialise them.
    (cd "$clone2" && "$OMEMFS" expand --recursive)

    local restored_hash
    restored_hash="$(sha256sum "$clone2/big.bin" | cut -d' ' -f1)"
    [ "$original_hash" = "$restored_hash" ]
    rm -rf "$clone2"
}

@test "chunk: pull downloads and assembles a chunked file via type-aware traversal" {
    # A second clone made before the chunked file is pushed; pulling it exercises
    # download_missing's BlobLeaf -> manifest -> chunk path.
    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    make_large_file big.bin $((20 * 1024 * 1024))
    local original_hash
    original_hash="$(sha256sum big.bin | cut -d' ' -f1)"
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    ( cd "$clone2" && "$OMEMFS" pull --stub-threshold 0 )
    local restored_hash
    restored_hash="$(sha256sum "$clone2/big.bin" | cut -d' ' -f1)"
    [ "$original_hash" = "$restored_hash" ]
    rm -rf "$clone2"
}

@test "chunk: lazy pull does not re-fetch chunks of an unchanged chunked file" {
    # Lazy pull (design/03): the diff short-circuits on equal subtree hashes, so
    # a pull that does not touch an already-materialised chunked file does NOT
    # traverse or re-fetch its chunks. The working-tree file is already correct,
    # so a chunk missing from the local cache is harmless and is not recovered.
    # (Recovery of a chunk genuinely needed for materialisation still happens on
    # demand — see `expand` and the materialise-on-pull paths.)
    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    make_large_file big.bin $((20 * 1024 * 1024))
    # Fix big.bin's mtime far in the past so it stays outside the STAT_CACHE
    # racy-clean window (RACY_THRESHOLD_SECS, design/07) for the whole test. A
    # file whose mtime is within a few seconds of "now" is treated as racily
    # clean and is re-hashed on every scan, which would re-chunk big.bin during
    # the second pull's working-tree scan and re-fetch the deleted chunk. With a
    # fixed past mtime the cache hits deterministically, so this test verifies
    # the lazy-pull short-circuit rather than racing the wall clock. (Before
    # build optimisation made push fast, the wall-clock gap alone kept big.bin
    # outside the racy window; the explicit backdate removes that timing
    # dependency.)
    touch -d "2020-01-01 00:00:00" big.bin
    echo "v1" > marker.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # clone2 downloads everything, including all chunks of big.bin.
    ( cd "$clone2" && "$OMEMFS" pull --stub-threshold 0 )
    local before
    before=$(count_objects_with_tag "$clone2/.omemfs/objects" "edf3")
    [ "$before" -ge 2 ]
    local big_hash
    big_hash="$(sha256sum "$clone2/big.bin" | cut -d' ' -f1)"

    # Simulate a partial cache: delete one chunk object from clone2's cache.
    local chunkfile
    chunkfile=$(find "$clone2/.omemfs/objects" -type f -print0 2>/dev/null \
        | while IFS= read -r -d '' f; do
            m=$(head -c2 "$f" | od -An -tx1 | tr -d ' \n')
            if [ "$m" = "edf3" ]; then echo "$f"; break; fi
          done)
    [ -n "$chunkfile" ]
    rm -f "$chunkfile"

    # A new remote change touches ONLY marker.txt; big.bin is unchanged.
    echo "v2" > marker.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    ( cd "$clone2" && "$OMEMFS" pull --stub-threshold 0 )
    # marker.txt updated.
    [ "$(cat "$clone2/marker.txt")" = "v2" ]
    # big.bin in the working tree is untouched and still correct.
    [ "$(sha256sum "$clone2/big.bin" | cut -d' ' -f1)" = "$big_hash" ]
    # The deleted chunk of the unchanged file was NOT re-fetched: lazy pull does
    # not traverse the unchanged big.bin subtree.
    local after
    after=$(count_objects_with_tag "$clone2/.omemfs/objects" "edf3")
    [ "$after" -lt "$before" ]
    rm -rf "$clone2"
}

@test "chunk: modify part of large file uploads only changed chunks on second push" {
    make_large_file big.bin $((20 * 1024 * 1024))

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Count remote objects after first push.
    local count_before
    count_before=$(find "$REMOTE_DIR/objects" -type f ! -name '.depth' ! -name '.migrating' 2>/dev/null | wc -l)

    # Overwrite just the first 4 bytes — only the first chunk changes boundary.
    printf '\xDE\xAD\xBE\xEF' | dd of=big.bin bs=1 count=4 conv=notrunc 2>/dev/null

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # New objects must be added (changed chunks + new manifest).
    local count_after
    count_after=$(find "$REMOTE_DIR/objects" -type f ! -name '.depth' ! -name '.migrating' 2>/dev/null | wc -l)
    [ "$count_after" -gt "$count_before" ]
}

@test "chunk: clone stub does not read the manifest for a multi-chunk blob" {
    # A multi-chunk file, pushed, then cloned with a stub threshold. Clone is
    # lazy: it stubs from the parent tree-entry metadata alone and downloads
    # NOTHING for a stubbed blob — not even the chunk manifest (StubRecord has
    # no `chunked` field to populate; see design/08_stub_system.md), matching
    # pull/restore/stub which also do not read the object.
    make_large_file big.bin $((20 * 1024 * 1024))
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    local hash
    hash="$("$OMEMFS" cat --hash clone-root/big.bin)"
    [ -n "$hash" ]

    local clone2
    clone2="$(mktemp -d)"
    # Threshold 1MiB: big.bin is far above it and gets stubbed.
    "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 1M "$clone2"
    [ -f "$clone2/big.bin.omemfs-stub" ]
    # The blob (and, if chunked, its manifest) must not have been fetched into
    # clone2's local object cache.
    ! object_exists_in "$clone2" "$hash"
    rm -rf "$clone2"
}

@test "chunk: large file (>= STREAMING_THRESHOLD) push/pull roundtrip via streaming write" {
    require_slow_tests

    # 65 MiB > 64 MiB STREAMING_THRESHOLD, so the one-pass streaming write path is
    # used. Content is pseudo-random so FastCDC produces multiple chunks.
    make_large_file huge.bin $((65 * 1024 * 1024))
    local original_hash
    original_hash="$(sha256sum huge.bin | cut -d' ' -f1)"

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Second push of the unchanged file is a no-op-ish: must not error.
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone2
    clone2="$(mktemp -d)"
    "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    # The large file is stubbed on clone; materialise it via expand.
    ( cd "$clone2" && "$OMEMFS" expand --recursive )
    [ -f "$clone2/huge.bin" ]

    local restored_hash
    restored_hash="$(sha256sum "$clone2/huge.bin" | cut -d' ' -f1)"
    [ "$original_hash" = "$restored_hash" ]
    rm -rf "$clone2"
}

@test "chunk: cat streams a multi-chunk file identically to the source" {
    # A multi-chunk file (5 MiB > CDC avg) cat'd to stdout must reproduce the
    # source byte for byte. Exercises the streaming for_each_blob_chunk path.
    make_large_file big.bin $((5 * 1024 * 1024))
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Resolve the blob hash for big.bin and cat it by hash.
    local hash
    hash="$("$OMEMFS" cat --hash clone-root/big.bin)"
    [ -n "$hash" ]

    "$OMEMFS" cat "$hash" > cat_out.bin
    cmp big.bin cat_out.bin
}

@test "chunk: clone stub does not fetch a non-chunked (single-object) blob" {
    # A small (single-object, non-chunked) blob above the stub threshold must
    # be stubbed without fetching the object -- same laziness claim as the
    # multi-chunk case above, exercised on the single-object code path.
    printf '%0100d' 0 > small_but_stubbed.txt   # 100 bytes, one object
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    local hash
    hash="$("$OMEMFS" cat --hash clone-root/small_but_stubbed.txt)"
    [ -n "$hash" ]

    local clone2
    clone2="$(mktemp -d)"
    "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 10 "$clone2"
    [ -f "$clone2/small_but_stubbed.txt.omemfs-stub" ]
    ! object_exists_in "$clone2" "$hash"
    rm -rf "$clone2"
}
