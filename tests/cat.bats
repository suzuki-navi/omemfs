#!/usr/bin/env bats
# Tests for `omemfs cat`

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

@test "cat: prints raw blob content" {
    printf 'hello world' > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Get the blob hash from ls output (format: "   hash size mtime path")
    local hash
    hash="$("$OMEMFS" ls --full-hash file.txt | awk '{print $1}')"
    run "$OMEMFS" cat "$hash"
    [ "$status" -eq 0 ]
    [ "$output" = "hello world" ]
}

@test "cat: pretty-prints tree JSON" {
    mkdir -p subdir
    echo "a" > subdir/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local hash
    hash="$(get_clone_root)"
    run "$OMEMFS" cat "$hash"
    [ "$status" -eq 0 ]
    [[ "$output" == *'"entries"'* ]]
    [[ "$output" == *'"kind"'* ]]
}

@test "cat: short prefix resolves to object" {
    printf 'short prefix test' > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local full_hash prefix
    full_hash="$("$OMEMFS" ls --full-hash file.txt | awk '{print $1}')"
    prefix="${full_hash:0:8}"
    run "$OMEMFS" cat "$prefix"
    [ "$status" -eq 0 ]
}

@test "cat: error on unknown hash" {
    run "$OMEMFS" cat "$(printf 'a%.0s' {1..64})"
    [ "$status" -ne 0 ]
    [[ "$output" == *"not found"* ]]
}

@test "cat: hash/path traverses tree" {
    mkdir -p docs
    printf 'guide content' > docs/guide.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local root_hash
    root_hash="$(get_clone_root)"
    run "$OMEMFS" cat "${root_hash}/docs/guide.md"
    [ "$status" -eq 0 ]
    [ "$output" = "guide content" ]
}

@test "cat: hash:path traverses tree (colon separator)" {
    mkdir -p docs
    printf 'guide content' > docs/guide.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local root_hash
    root_hash="$(get_clone_root)"
    run "$OMEMFS" cat "${root_hash}:docs/guide.md"
    [ "$status" -eq 0 ]
    [ "$output" = "guide content" ]
}

@test "cat: short-hash:path traverses tree (colon + prefix)" {
    mkdir -p docs
    printf 'prefix colon content' > docs/guide.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local root_hash short
    root_hash="$(get_clone_root)"
    short="${root_hash:0:8}"
    run "$OMEMFS" cat "${short}:docs/guide.md"
    [ "$status" -eq 0 ]
    [ "$output" = "prefix colon content" ]
}

@test "cat: colon and slash separators give identical output" {
    mkdir -p docs
    printf 'sep equivalence' > docs/guide.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local root_hash colon_out slash_out
    root_hash="$(get_clone_root)"
    colon_out="$("$OMEMFS" cat "${root_hash}:docs/guide.md")"
    slash_out="$("$OMEMFS" cat "${root_hash}/docs/guide.md")"
    [ "$colon_out" = "$slash_out" ]
}

@test "cat: clone-root:path traverses tree (alias + colon)" {
    mkdir -p docs
    printf 'alias colon traversal' > docs/readme.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" cat "clone-root:docs/readme.md"
    [ "$status" -eq 0 ]
    [ "$output" = "alias colon traversal" ]
}

@test "cat: clone-root prints root tree JSON" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" cat clone-root
    [ "$status" -eq 0 ]
    [[ "$output" == *'"entries"'* ]]
    [[ "$output" == *'"kind"'* ]]
}

@test "cat: remote-root prints remote root tree JSON" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" cat remote-root
    [ "$status" -eq 0 ]
    [[ "$output" == *'"entries"'* ]]
    [[ "$output" == *'"kind"'* ]]
}

@test "cat: clone-root output matches cat of clone_root hash" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local hash_output alias_output
    hash_output="$("$OMEMFS" cat "$(get_clone_root)")"
    alias_output="$("$OMEMFS" cat clone-root)"
    [ "$hash_output" = "$alias_output" ]
}

@test "cat: clone-root/path traverses tree" {
    mkdir -p docs
    printf 'alias traversal' > docs/readme.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" cat "clone-root/docs/readme.md"
    [ "$status" -eq 0 ]
    [ "$output" = "alias traversal" ]
}

@test "cat: remote-root/path traverses tree" {
    mkdir -p docs
    printf 'remote traversal' > docs/readme.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" cat "remote-root/docs/readme.md"
    [ "$status" -eq 0 ]
    [ "$output" = "remote traversal" ]
}

@test "cat: clone-root fails before first push" {
    run "$OMEMFS" cat clone-root
    [ "$status" -ne 0 ]
    [[ "$output" == *"no clone_root"* ]]
}

@test "cat: --hash prints 64-char hash for clone-root" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" cat --hash clone-root
    [ "$status" -eq 0 ]
    [ "${#output}" -eq 64 ]
    [[ "$output" =~ ^[0-9a-f]{64}$ ]]
}

@test "cat: --hash output matches clone_root file" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local expected
    expected="$(get_clone_root)"
    run "$OMEMFS" cat --hash clone-root
    [ "$status" -eq 0 ]
    [ "$output" = "$expected" ]
}

@test "cat: --hash for remote-root matches REMOTE_ROOT" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local expected
    expected="$(get_remote_root)"
    run "$OMEMFS" cat --hash remote-root
    [ "$status" -eq 0 ]
    [ "$output" = "$expected" ]
}

@test "cat: --hash with path prints leaf object hash" {
    mkdir -p docs
    printf 'guide content' > docs/guide.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local blob_hash
    blob_hash="$("$OMEMFS" ls --full-hash docs/guide.md | awk '{print $1}')"
    run "$OMEMFS" cat --hash "clone-root/docs/guide.md"
    [ "$status" -eq 0 ]
    [ "$output" = "$blob_hash" ]
}

@test "cat: --hash with hash/path prints leaf object hash" {
    mkdir -p docs
    printf 'guide content' > docs/guide.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local root_hash blob_hash
    root_hash="$(get_clone_root)"
    blob_hash="$("$OMEMFS" ls --full-hash docs/guide.md | awk '{print $1}')"
    run "$OMEMFS" cat --hash "${root_hash}/docs/guide.md"
    [ "$status" -eq 0 ]
    [ "$output" = "$blob_hash" ]
}

# ---------------------------------------------------------------------------
# index-root and remote pack-layer fallback
# ---------------------------------------------------------------------------

@test "cat: index-root prints JSON with remote_root field" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" cat index-root
    [ "$status" -eq 0 ]
    [[ "$output" == *'"remote_root"'* ]]
    [[ "$output" == *'"hot_hash"'* ]]
    [[ "$output" == *'"delta_hashes"'* ]]
    [[ "$output" == *'"cold_shards"'* ]]
}

@test "cat: index-root remote_root matches REMOTE_ROOT" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local remote_root index_root_value
    remote_root="$(get_remote_root)"
    index_root_value="$("$OMEMFS" cat index-root | grep '"remote_root"' | grep -oE '[0-9a-f]{64}')"
    [ "$remote_root" = "$index_root_value" ]
}

@test "cat: index-root fails when index root does not exist" {
    # Never pushed — no index root in remote.
    run "$OMEMFS" cat index-root
    [ "$status" -ne 0 ]
    [[ "$output" == *"no index root on"* ]]
}

@test "cat: index-root with --hash returns error" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" cat --hash index-root
    [ "$status" -ne 0 ]
    [[ "$output" == *"--hash is not supported"* ]]
}

@test "cat: index-root with --remote uses specified remote" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" cat --remote origin index-root
    [ "$status" -eq 0 ]
    [[ "$output" == *'"remote_root"'* ]]
}

@test "cat: index-root contains cold_prefix_bits field" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" cat index-root
    [ "$status" -eq 0 ]
    [[ "$output" == *'"cold_prefix_bits"'* ]]
}

@test "cat: hash of remote index file prints entry_count" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Obtain the delta index hash from index-root JSON.
    local delta_hash
    delta_hash="$("$OMEMFS" cat index-root | grep -A1 '"delta_hashes"' | grep -oE '[0-9a-f]{64}' | head -1)"
    [ -n "$delta_hash" ]

    run "$OMEMFS" cat "${delta_hash}"
    [ "$status" -eq 0 ]
    [[ "$output" == *'"entry_count"'* ]]
    [[ "$output" == *'"entries"'* ]]
}

@test "cat: hash of remote index file shows correct entry types" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local delta_hash
    delta_hash="$("$OMEMFS" cat index-root | grep -A1 '"delta_hashes"' | grep -oE '[0-9a-f]{64}' | head -1)"
    [ -n "$delta_hash" ]

    run "$OMEMFS" cat "${delta_hash}"
    [ "$status" -eq 0 ]
    # A small file should be an inline or pack entry.
    [[ "$output" == *'"type"'* ]]
}

@test "cat: hash of remote bloom filter prints fill_rate" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local bloom_hash
    bloom_hash="$("$OMEMFS" cat index-root | grep '"bloom_hash"' | grep -oE '[0-9a-f]{64}')"
    [ -n "$bloom_hash" ]

    run "$OMEMFS" cat "${bloom_hash}"
    [ "$status" -eq 0 ]
    [[ "$output" == *'"num_hash_functions"'* ]]
    [[ "$output" == *'"num_bits"'* ]]
    [[ "$output" == *'"element_count"'* ]]
    [[ "$output" == *'"fill_rate"'* ]]
}

@test "cat: hash of remote bloom filter prints designed_capacity and estimated_fp_rate" {
    # design/02_storage_format.md "Fill-rate reporting": a loaded filter's
    # fill state must be reportable from the fields it already carries
    # (element_count, num_bits), without any format change.
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local bloom_hash
    bloom_hash="$("$OMEMFS" cat index-root | grep '"bloom_hash"' | grep -oE '[0-9a-f]{64}')"
    [ -n "$bloom_hash" ]

    run "$OMEMFS" cat "${bloom_hash}"
    [ "$status" -eq 0 ]
    [[ "$output" == *'"designed_capacity"'* ]]
    [[ "$output" == *'"estimated_fp_rate"'* ]]

    local designed_capacity estimated_fp_rate num_bits
    designed_capacity="$(echo "$output" \
        | python -c "import sys,json; d=json.load(sys.stdin); print(d['designed_capacity'])")"
    estimated_fp_rate="$(echo "$output" \
        | python -c "import sys,json; d=json.load(sys.stdin); print(d['estimated_fp_rate'])")"
    num_bits="$(echo "$output" \
        | python -c "import sys,json; d=json.load(sys.stdin); print(d['num_bits'])")"

    # A fresh push-time filter is sized at MIN_CAPACITY (100_000); its
    # designed_capacity should reflect a comparably large figure, not zero
    # or a tiny mismatched value.
    [ "$designed_capacity" -gt 50000 ]
    [ "$num_bits" -gt 0 ]
    # estimated_fp_rate is a probability in [0, 1]; for a lightly-loaded
    # fresh filter it must be very small (well under the 1% design target).
    python3 -c "assert 0.0 <= float('$estimated_fp_rate') < 0.01, '$estimated_fp_rate out of expected range'"
}

@test "cat: remote pack-layer hash with unknown full hash returns error" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" cat "$(printf 'a%.0s' {1..64})"
    [ "$status" -ne 0 ]
}

@test "cat: remote index delta hash entries contain valid 64-char hashes" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local delta_hash
    delta_hash="$("$OMEMFS" cat index-root \
        | python -c "import sys,json; d=json.load(sys.stdin); print(d['delta_hashes'][0])")"
    [ -n "$delta_hash" ]

    local invalid_count
    invalid_count="$("$OMEMFS" cat "${delta_hash}" \
        | python -c "
import sys, json, re
d = json.load(sys.stdin)
count = 0
for e in d['entries']:
    h = e.get('hash', '')
    if h and not re.fullmatch(r'[0-9a-f]{64}', h):
        count += 1
print(count)
")"
    [ "$invalid_count" -eq 0 ]
}

@test "cat: binary blob with control bytes round-trips byte-exact" {
    # Regression guard for the phase-view / output ordering fix: blob content is
    # streamed raw to stdout after the phase view is sealed, so bytes that look
    # like ANSI escapes or newlines must pass through untouched.
    printf 'a\x1b[31m\x00\nb\x1b[0m\x07c' > bin.dat
    local original_hash
    original_hash="$(sha256sum bin.dat | cut -d' ' -f1)"

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local hash
    hash="$("$OMEMFS" ls --full-hash bin.dat | awk '{print $1}')"
    "$OMEMFS" cat "$hash" > out.dat
    local restored_hash
    restored_hash="$(sha256sum out.dat | cut -d' ' -f1)"
    [ "$original_hash" = "$restored_hash" ]
}

@test "cat: large multi-chunk blob round-trips byte-exact" {
    # A >4 MiB file is split into multiple chunks; cat must reassemble and stream
    # them in order without corruption or interleaved progress output.
    head -c 5242880 /dev/urandom > big.bin
    local original_hash
    original_hash="$(sha256sum big.bin | cut -d' ' -f1)"

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local hash
    hash="$("$OMEMFS" ls --full-hash big.bin | awk '{print $1}')"
    "$OMEMFS" cat "$hash" > out.bin
    local restored_hash
    restored_hash="$(sha256sum out.bin | cut -d' ' -f1)"
    [ "$original_hash" = "$restored_hash" ]
}

# ---------------------------------------------------------------------------
# LiveFallback: `cat <full-hash>` on an object outside the current snapshot
# (design/02_storage_format.md, "Snapshot read path"; design/04_cli_spec.md,
# "Resolution order for hash targets", step 3).
# ---------------------------------------------------------------------------

@test "cat: LiveFallback resolves an orphaned standalone object and warns on stderr" {
    # Produce a well-formed standalone-sized (>= 1 MiB) object via a real push
    # into a disposable donor clone/remote, so the harvested stored bytes are
    # exactly what a real omemfs write would produce (encode pipeline and all).
    local donor_dir donor_remote orphan_hash
    donor_dir="$(mktemp -d)"
    donor_remote="$(mktemp -d)"
    (
        cd "$donor_dir"
        "$OMEMFS" clone --new --url "$donor_remote" . >/dev/null
        head -c 1048577 /dev/urandom > orphan.bin
        "$OMEMFS" push >/dev/null
    )
    orphan_hash="$(cd "$donor_dir" && "$OMEMFS" ls --full-hash orphan.bin | awk '{print $1}')"

    # Standalone objects are written directly at objects/<hash> on the remote
    # (src/codec/pack/writer.rs `route()`). All remote backends use the fixed
    # 3-level sharding in src/store/remote_objects_dir.rs (NOT the local
    # cache's adaptive-depth layout): objects/<2>/<2>/<2>/<rest>.
    local p1="${orphan_hash:0:2}" p2="${orphan_hash:2:2}" p3="${orphan_hash:4:2}" rest="${orphan_hash:6}"
    local donor_obj_path="$donor_remote/objects/$p1/$p2/$p3/$rest"
    [ -f "$donor_obj_path" ]

    # This test's own repo (from setup_repo) already has a remote at
    # $REMOTE_DIR. Push unrelated content first so an INDEX_ROOT exists here
    # too, then splice in the donor's raw bytes under the SAME hash, with no
    # index entry anywhere referencing it in THIS remote -- a genuine orphan,
    # e.g. from an interrupted or obsolete write.
    echo "unrelated tracked content" > tracked.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    local target_obj_path="$REMOTE_DIR/objects/$p1/$p2/$p3/$rest"
    mkdir -p "$(dirname "$target_obj_path")"
    cp "$donor_obj_path" "$target_obj_path"

    "$OMEMFS" cat "$orphan_hash" > out.bin 2> err.txt
    local cat_status=$?
    [ "$cat_status" -eq 0 ]

    # stdout carries only the decoded content, byte-exact.
    cmp "$donor_dir/orphan.bin" out.bin

    # The warning is on stderr only, and identifies the object as outside the
    # snapshot -- never on stdout, so scripts piping cat's stdout are unaffected.
    grep -q "outside the current snapshot" err.txt
    ! grep -aq "outside the current snapshot" out.bin

    rm -rf "$donor_dir" "$donor_remote" out.bin err.txt

    # ls/pull against this remote must be completely unaffected: no error, and
    # the orphan hash never appears as if it were part of the remote-tracked
    # tree. `--remote` scans the remote's tree (walked from its root), as
    # opposed to the working tree, which would otherwise show `out.bin`
    # (removed above) as a false positive since it has identical content.
    run "$OMEMFS" ls -r --remote --full-hash .
    [ "$status" -eq 0 ]
    [[ "$output" != *"$orphan_hash"* ]]

    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
}

# ---------------------------------------------------------------------------
# --manifest (design/04_cli_spec.md, "omemfs cat", "Chunk manifest output")
# ---------------------------------------------------------------------------

@test "cat: --manifest reports chunk_count and hex chunk hashes for a multi-chunk blob" {
    # 20 MiB reliably produces >= 2 chunks (FastCDC's cut points are content-
    # defined with substantial variance around avg_size (4 MiB), so a file
    # only slightly above avg_size can land as a single chunk; 20 MiB is the
    # same size tests/chunk.bats already relies on for a >= 2 assertion).
    head -c $((20 * 1024 * 1024)) /dev/urandom > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local hash
    hash="$("$OMEMFS" ls --full-hash big.bin | awk '{print $1}')"
    run "$OMEMFS" cat --manifest "$hash"
    [ "$status" -eq 0 ]

    local chunk_count array_len invalid_count
    chunk_count="$(echo "$output" | python -c "import sys,json; print(json.load(sys.stdin)['chunk_count'])")"
    array_len="$(echo "$output" | python -c "import sys,json; print(len(json.load(sys.stdin)['chunks']))")"
    [ "$chunk_count" -ge 2 ]
    [ "$chunk_count" -eq "$array_len" ]

    invalid_count="$(echo "$output" | python -c "
import sys, json, re
d = json.load(sys.stdin)
print(sum(1 for h in d['chunks'] if not re.fullmatch(r'[0-9a-f]{64}', h)))
")"
    [ "$invalid_count" -eq 0 ]
}

@test "cat: --manifest chunk order reconstructs the original file byte-exact" {
    # This is the correctness core of --manifest: the chunk list must be in
    # storage/reassembly order, never reordered.
    #
    # The first byte is fixed to 'A' (not in the ED F0..FF blob-escape range)
    # so the assembled FastCDC input never gets the ED F0 escape prefix
    # (design/02_storage_format.md, "Streaming design": the escape is
    # prepended to the whole stream before chunking, so it would otherwise
    # land inside chunk 0's bytes and complicate this test's byte accounting).
    # 20 MiB reliably produces >= 2 chunks; see the size comment above.
    { printf 'A'; head -c $((20 * 1024 * 1024 - 1)) /dev/urandom; } > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local hash
    hash="$("$OMEMFS" ls --full-hash big.bin | awk '{print $1}')"
    run "$OMEMFS" cat --manifest "$hash"
    [ "$status" -eq 0 ]
    local manifest_json="$output"

    local chunk_count
    chunk_count="$(echo "$manifest_json" | python -c "import sys,json; print(json.load(sys.stdin)['chunk_count'])")"
    [ "$chunk_count" -ge 2 ]

    : > reconstructed.bin
    local i chash
    for ((i = 0; i < chunk_count; i++)); do
        chash="$(echo "$manifest_json" | python -c "import sys,json; print(json.load(sys.stdin)['chunks'][$i])")"
        [[ "$chash" =~ ^[0-9a-f]{64}$ ]]
        # Each chunk is stored standalone as `ED F3 | chunk_bytes`
        # (src/object.rs TYPE_TAG_CHUNK). A plain `cat` of a chunk hash treats
        # it as an ordinary (unchunked) blob -- deserialise_blob only strips
        # the ED F0 blob-escape, not ED F3 -- so the raw output carries the
        # 2-byte ED F3 tag unstripped. Strip it ourselves before reassembling.
        "$OMEMFS" cat "$chash" > chunk_raw.bin
        tail -c +3 chunk_raw.bin >> reconstructed.bin
    done

    cmp big.bin reconstructed.bin
    rm -f chunk_raw.bin reconstructed.bin
}

@test "cat: --manifest for a small unchunked blob reports itself as the sole chunk" {
    printf 'small manifest content' > small.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local hash
    hash="$("$OMEMFS" ls --full-hash small.txt | awk '{print $1}')"
    run "$OMEMFS" cat --manifest "$hash"
    [ "$status" -eq 0 ]

    local chunk_count sole_chunk manifest_hash
    chunk_count="$(echo "$output" | python -c "import sys,json; print(json.load(sys.stdin)['chunk_count'])")"
    sole_chunk="$(echo "$output" | python -c "import sys,json; print(json.load(sys.stdin)['chunks'][0])")"
    manifest_hash="$(echo "$output" | python -c "import sys,json; print(json.load(sys.stdin)['hash'])")"
    [ "$chunk_count" -eq 1 ]
    [ "$sole_chunk" = "$hash" ]
    [ "$manifest_hash" = "$hash" ]
}

@test "cat: --manifest on a tree object errors" {
    mkdir -p subdir
    echo "a" > subdir/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" cat --manifest clone-root
    [ "$status" -ne 0 ]
    [[ "$output" == *"not applicable to a tree object"* ]]
}

@test "cat: --hash and --manifest together is rejected by clap" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local hash
    hash="$("$OMEMFS" ls --full-hash file.txt | awk '{print $1}')"
    run "$OMEMFS" cat --hash --manifest "$hash"
    [ "$status" -ne 0 ]
    [[ "$output" == *"--hash"* ]]
    [[ "$output" == *"--manifest"* ]]
}

@test "cat: <ref>:<path> traversal works with --manifest" {
    head -c $((20 * 1024 * 1024)) /dev/urandom > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local blob_hash
    blob_hash="$("$OMEMFS" ls --full-hash big.bin | awk '{print $1}')"
    run "$OMEMFS" cat --manifest "clone-root:big.bin"
    [ "$status" -eq 0 ]

    local reported_hash chunk_count
    reported_hash="$(echo "$output" | python -c "import sys,json; print(json.load(sys.stdin)['hash'])")"
    chunk_count="$(echo "$output" | python -c "import sys,json; print(json.load(sys.stdin)['chunk_count'])")"
    [ "$reported_hash" = "$blob_hash" ]
    [ "$chunk_count" -ge 2 ]
}

@test "cat: --manifest resolves a full hash via SnapshotOnly when absent from the local cache" {
    head -c $((20 * 1024 * 1024)) /dev/urandom > big.bin
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local hash
    hash="$("$OMEMFS" ls --full-hash big.bin | awk '{print $1}')"

    # A fresh clone from the same remote, never pulled: its local object cache
    # has nothing beyond the initial clone_root, so resolving this full hash
    # exercises the remote SnapshotOnly index lookup (design/04_cli_spec.md,
    # "Resolution order for hash targets", step 2), not the local cache.
    local clone2
    clone2="$(mktemp -d)"
    "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    run bash -c "cd '$clone2' && '$OMEMFS' cat --manifest '$hash'"
    [ "$status" -eq 0 ]

    local chunk_count
    chunk_count="$(echo "$output" | python -c "import sys,json; print(json.load(sys.stdin)['chunk_count'])")"
    [ "$chunk_count" -ge 2 ]
    rm -rf "$clone2"
}

@test "cat: --manifest works through LiveFallback and still warns on stderr" {
    # Same orphan-splicing setup as "cat: LiveFallback resolves an orphaned
    # standalone object and warns on stderr" above, but exercising --manifest
    # instead of plain content output. The orphan is a single standalone
    # object (>= 1 MiB, below the ~4 MiB chunking average), so it is
    # unchunked: --manifest reports it as its own sole chunk.
    local donor_dir donor_remote orphan_hash
    donor_dir="$(mktemp -d)"
    donor_remote="$(mktemp -d)"
    (
        cd "$donor_dir"
        "$OMEMFS" clone --new --url "$donor_remote" . >/dev/null
        head -c 1048577 /dev/urandom > orphan.bin
        "$OMEMFS" push >/dev/null
    )
    orphan_hash="$(cd "$donor_dir" && "$OMEMFS" ls --full-hash orphan.bin | awk '{print $1}')"

    local p1="${orphan_hash:0:2}" p2="${orphan_hash:2:2}" p3="${orphan_hash:4:2}" rest="${orphan_hash:6}"
    local donor_obj_path="$donor_remote/objects/$p1/$p2/$p3/$rest"
    [ -f "$donor_obj_path" ]

    echo "unrelated tracked content" > tracked.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    local target_obj_path="$REMOTE_DIR/objects/$p1/$p2/$p3/$rest"
    mkdir -p "$(dirname "$target_obj_path")"
    cp "$donor_obj_path" "$target_obj_path"

    "$OMEMFS" cat --manifest "$orphan_hash" > out.json 2> err.txt
    local cat_status=$?
    [ "$cat_status" -eq 0 ]

    # stdout carries only the manifest JSON.
    local chunk_count sole_chunk manifest_hash
    chunk_count="$(python -c "import sys,json; print(json.load(open('out.json'))['chunk_count'])")"
    sole_chunk="$(python -c "import sys,json; print(json.load(open('out.json'))['chunks'][0])")"
    manifest_hash="$(python -c "import sys,json; print(json.load(open('out.json'))['hash'])")"
    [ "$chunk_count" -eq 1 ]
    [ "$sole_chunk" = "$orphan_hash" ]
    [ "$manifest_hash" = "$orphan_hash" ]

    # The LiveFallback warning is still printed to stderr, never stdout.
    grep -q "outside the current snapshot" err.txt
    ! grep -aq "outside the current snapshot" out.json

    rm -rf "$donor_dir" "$donor_remote" out.json err.txt
}

@test "cat: a blob whose content is well-formed tree JSON is printed raw, not pretty-printed as a tree" {
    # D13 (design/01_object_model.md "Tree/blob discrimination"): this
    # object has no ED F1 tag -- it is an ordinary blob -- but its content
    # happens to be valid tree JSON. Before Tree::is_tree_bytes, cat's "is
    # this a tree?" check (Tree::deserialise succeeding) misidentified it
    # as a tree and pretty-printed the parsed Tree value instead of
    # streaming the blob's actual bytes.
    printf '{"kind":"normal","entries":[]}' > tree_shaped.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local hash
    hash="$("$OMEMFS" ls --full-hash tree_shaped.txt | awk '{print $1}')"
    run "$OMEMFS" cat "$hash"
    [ "$status" -eq 0 ]
    [ "$output" = '{"kind":"normal","entries":[]}' ]
}
