#!/usr/bin/env bats
# Tests for `omemfs clone`

load test_helper/common

setup() {
    setup_test_dir
    setup_local_remote
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# ---------------------------------------------------------------------------
# Empty remote
# ---------------------------------------------------------------------------

@test "clone: creates .omemfs/ directory" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" myrepo
    [ "$status" -eq 0 ]
    [ -d myrepo/.omemfs ]
}

@test "clone: creates config file" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" myrepo
    [ "$status" -eq 0 ]
    [ -f myrepo/.omemfs/config ]
}

@test "clone: creates clone_root file" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" myrepo
    [ "$status" -eq 0 ]
    [ -f myrepo/.omemfs/clone_root ] || \
        [ "$(cat myrepo/.omemfs/clone_root 2>/dev/null)" = "" ]
}

@test "clone: empty remote succeeds without error" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" myrepo
    [ "$status" -eq 0 ]
}

@test "clone: refuses to clone into non-empty directory without --force" {
    mkdir -p nonempty
    touch nonempty/existing.txt
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" nonempty
    [ "$status" -ne 0 ]
    [[ "$output" == *"not empty"* ]]
}

@test "clone: --force allows cloning into non-empty directory" {
    mkdir -p nonempty
    touch nonempty/existing.txt
    run "$OMEMFS" clone --new --force --url "$REMOTE_DIR" nonempty
    [ "$status" -eq 0 ]
}

@test "clone: --force skips existing paths and leaves their content untouched" {
    # Seed the remote with a file via a first clone+push.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" seed
    [ "$status" -eq 0 ]
    ( cd seed && echo "remote content" > file.txt && "$OMEMFS" push )
    [ "$status" -eq 0 ]

    # Pre-create the destination with a DIFFERENT content at the same path.
    mkdir -p dest
    echo "local content" > dest/file.txt

    run "$OMEMFS" clone --existing --force --url "$REMOTE_DIR" dest
    [ "$status" -eq 0 ]
    # Existing path must be skipped, not overwritten.
    [ "$(cat dest/file.txt)" = "local content" ]
    [[ "$output" == *"existing path(s) skipped"* ]]
}

@test "clone: default remote name is 'origin'" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" myrepo
    [ "$status" -eq 0 ]
    grep -q '"origin"' myrepo/.omemfs/config
}

@test "clone: restores mtime from tree entry metadata" {
    # Push a file with a specific mtime into the remote via clone1.
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    cd clone1
    echo "hello" > file.txt
    touch -t 200001010000.00 file.txt
    local original_mtime
    original_mtime="$(stat -c '%Y' file.txt)"
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # Clone into clone2 and verify the mtime is restored to the original.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]
    local restored_mtime
    restored_mtime="$(stat -c '%Y' clone2/file.txt)"
    [ "$restored_mtime" -eq "$original_mtime" ]
}

# ---------------------------------------------------------------------------
# Executable-bit (mode) restoration
# ---------------------------------------------------------------------------

@test "clone: executable bit is restored on materialised files" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    echo "#!/bin/sh" > script.sh
    chmod +x script.sh
    echo "plain" > plain.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" dest_repo
    [ "$status" -eq 0 ]
    [ -x dest_repo/script.sh ]
    [ ! -x dest_repo/plain.txt ]
    # The fresh clone must be clean (tree hash includes mode).
    cd dest_repo
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

# ---------------------------------------------------------------------------
# Reserved-name rejection in remote tree entries (design/09_reserved_names.md,
# "Reserved names in remote tree entries (untrusted input)"; design/01
# "Invariants" #5). A tampered/hostile remote could publish a root tree whose
# entry is named ".omemfs" (would collide with the metadata directory once
# materialised) -- `clone --existing` must refuse it via
# `Tree::deserialise`'s reserved-name check rather than writing it into the
# destination working tree.
# ---------------------------------------------------------------------------

# Hand-craft a minimal well-formed tree object with a single blob entry named
# $2, register it in a synthetic delta index file, prepend that delta to
# INDEX_ROOT's delta_hashes, and set remote_root to the tree's hash -- so a
# subsequent `clone --existing` resolves this crafted tree as the remote root
# through the normal pack-index read path (`PackReader::locate`), exactly
# like a real pushed tree would be. Writing the tree object bytes directly
# under objects/ without an index entry is NOT enough: reads go through the
# delta/hot/cold index (src/codec/pack/reader.rs `locate`), which never falls
# back to a raw objects/ scan. Assumes an unencrypted remote (INDEX_ROOT is
# plaintext) with no existing cold shards beyond the default single all-zero
# slot (true for a fresh `clone --new` remote, as used by every caller of
# this helper). Same technique as `craft_malicious_remote_root` in
# tests/pull.bats (duplicated here since each bats file loads independently).
craft_malicious_remote_root() {
    local remote_dir="$1" entry_name="$2"
    python3 - "$remote_dir" "$entry_name" <<'PY'
import hashlib, json, os, struct, sys

remote_dir, entry_name = sys.argv[1], sys.argv[2]

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

inline_entry = bytes([0x01]) + tree_hash + bytes([len(tree_serialised)]) + tree_serialised
delta_bytes = bytes([0xED, 0xE2, 0x01, 0x00]) + struct.pack(">I", 1) + inline_entry
delta_hash = hashlib.sha256(delta_bytes).digest()
write_obj(remote_dir, delta_hash.hex(), delta_bytes)

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

@test "clone: --existing refuses a remote root tree with a .omemfs entry name" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src_repo
    [ "$status" -eq 0 ]
    cd src_repo
    echo "seed" > seed.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    craft_malicious_remote_root "$REMOTE_DIR" ".omemfs"

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" dest_repo
    [ "$status" -ne 0 ]
    [[ "$output" == *"invalid object"* ]]
    # The clone must not have gone on to write a colliding ".omemfs" path.
    [ ! -d dest_repo ] || [ ! -e dest_repo/.omemfs/.omemfs ]
}
