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
