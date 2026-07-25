#!/usr/bin/env bats
# Tests for client-side encryption (AES-256-GCM)

load test_helper/common

setup() {
    setup_test_dir
    setup_local_remote
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

@test "encrypt: clone --encrypt on empty remote writes encryption config" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" --encrypt .
    [ "$status" -eq 0 ]
    run grep -q '"algorithm"' .omemfs/config
    [ "$status" -eq 0 ]
    run grep -q '"dek"' .omemfs/config
    [ "$status" -eq 0 ]
}

@test "encrypt: push and pull roundtrip with encryption" {
    "$OMEMFS" clone --new --url "$REMOTE_DIR" --encrypt .
    echo "secret content" > secret.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Simulate a fresh clone by resetting clone_root, then pull
    echo "" > .omemfs/clone_root
    rm secret.txt
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ -f "secret.txt" ]
    run grep -q "secret content" secret.txt
    [ "$status" -eq 0 ]
}

@test "encrypt: remote object bytes differ from plaintext" {
    "$OMEMFS" clone --new --url "$REMOTE_DIR" --encrypt .
    echo "plaintext data" > data.txt
    "$OMEMFS" push

    # Find any object file in the remote
    local obj_file
    obj_file="$(find "$REMOTE_DIR/objects" -type f | head -n 1)"
    [ -n "$obj_file" ]

    # The stored bytes must not contain the plaintext string
    run grep -rq "plaintext data" "$REMOTE_DIR/objects"
    [ "$status" -ne 0 ]
}

@test "encrypt: unencrypted clone still works without --encrypt" {
    "$OMEMFS" clone --new --url "$REMOTE_DIR" .
    echo "normal file" > normal.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run grep -q '"dek"' .omemfs/config
    [ "$status" -ne 0 ]
}

@test "encrypt: second clone of encrypted repo with correct DEK downloads files" {
    # First clone: create an encrypted repository and push a file.
    local CLONE_A="$TEST_DIR/clone_a"
    mkdir "$CLONE_A"
    "$OMEMFS" clone --new --url "$REMOTE_DIR" --encrypt "$CLONE_A"
    echo "encrypted content" > "$CLONE_A/secret.txt"
    (cd "$CLONE_A" && "$OMEMFS" push)

    # Extract the DEK from clone_a's config for reuse.
    local DEK
    DEK="$(python3 -c "import json,sys; c=json.load(open('$CLONE_A/.omemfs/config')); print(c['remotes']['origin']['encryption']['dek'])")"

    # Export the connection string (encodes the full config including the DEK).
    local CONN_STR
    CONN_STR="$( (cd "$CLONE_A" && "$OMEMFS" config export) 2>/dev/null | grep '^omemfs_repo_')"

    # Second clone: use the connection string so the DEK is preserved automatically.
    local CLONE_B="$TEST_DIR/clone_b"
    mkdir "$CLONE_B"
    run "$OMEMFS" clone --url "$CONN_STR" "$CLONE_B"
    [ "$status" -eq 0 ]

    # The config must contain the same DEK.
    run grep -q '"dek"' "$CLONE_B/.omemfs/config"
    [ "$status" -eq 0 ]
    local DEK_B
    DEK_B="$(python3 -c "import json,sys; c=json.load(open('$CLONE_B/.omemfs/config')); print(c['remotes']['origin']['encryption']['dek'])")"
    [ "$DEK" = "$DEK_B" ]

    # The file must have been restored correctly.
    [ -f "$CLONE_B/secret.txt" ]
    run grep -q "encrypted content" "$CLONE_B/secret.txt"
    [ "$status" -eq 0 ]
}

@test "encrypt: --encrypt on a non-empty remote fails (implies new)" {
    # First clone sets up the remote with encrypted data.
    "$OMEMFS" clone --new --url "$REMOTE_DIR" --encrypt .
    echo "data" > file.txt
    "$OMEMFS" push

    # A second --encrypt (which implies --new) on the same non-empty remote
    # must fail the new-repository emptiness check.
    local CLONE_B="$TEST_DIR/clone_b"
    mkdir "$CLONE_B"
    run "$OMEMFS" clone --url "$REMOTE_DIR" --encrypt "$CLONE_B"
    [ "$status" -ne 0 ]
    [[ "$output" =~ "remote prefix is not empty" ]]
}

@test "encrypt: corrupting a stored object fails the read with a clear error (no garbage)" {
    # Encrypted repo; push a large file so it is stored as a standalone object
    # directly under objects/ (the largest file in the remote).
    local CLONE_A="$TEST_DIR/clone_a"
    mkdir "$CLONE_A"
    "$OMEMFS" clone --new --url "$REMOTE_DIR" --encrypt "$CLONE_A"
    head -c 2000000 /dev/urandom > "$CLONE_A/big.bin"
    (cd "$CLONE_A" && "$OMEMFS" push)

    # Corrupt one byte in the largest remote object (the standalone blob).
    local obj_file
    obj_file="$(find "$REMOTE_DIR/objects" -type f ! -name '.depth' ! -name '.migrating' \
        -printf '%s %p\n' | sort -rn | head -n1 | cut -d' ' -f2-)"
    [ -n "$obj_file" ]
    python - "$obj_file" <<'PY'
import sys
p = sys.argv[1]
with open(p, "rb") as f:
    data = bytearray(f.read())
# Flip a byte well inside the ciphertext (not the appended tag region).
data[10] ^= 0xFF
with open(p, "wb") as f:
    f.write(data)
PY

    # Fresh clone with the same DEK (via connection string) must fail to
    # materialise the corrupted object — never write garbage. Clone is lazy and
    # would stub a 2 MB file at the default threshold (never reading it), so
    # --stub-threshold 0 is used to force materialisation and exercise the
    # decrypt path that detects the tampering.
    local CONN_STR
    CONN_STR="$( (cd "$CLONE_A" && "$OMEMFS" config export) 2>/dev/null | grep '^omemfs_repo_')"
    local CLONE_B="$TEST_DIR/clone_b"
    mkdir "$CLONE_B"
    run "$OMEMFS" clone --url "$CONN_STR" --stub-threshold 0 "$CLONE_B"
    [ "$status" -ne 0 ]
    [[ "$output" == *"authentication tag mismatch"* ]] || [[ "$output" == *"corrupted or tampered"* ]]
    # The corrupted content must never have been written to the working tree.
    [ ! -f "$CLONE_B/big.bin" ] || [ ! -s "$CLONE_B/big.bin" ]
}
