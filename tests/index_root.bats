#!/usr/bin/env bats
# Tests for the derived index-root key on encrypted remotes, the clone
# new/existing declaration + validation, and the post-clone sync guard.
#
# See design/02 (Index root name derivation), design/03 (Post-clone sync
# guard), design/04 (clone Options / New-existing declaration / Validation).

load test_helper/common

setup() {
    setup_test_dir
    setup_local_remote
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# Compute the encrypted-remote derived index-root object path for a repo whose
# DEK lives in <repo>/.omemfs/config, rooted at $REMOTE_DIR. Prints the path.
derived_index_root_path() {
    local repo="$1"
    python - "$repo" "$REMOTE_DIR" <<'PY'
import json, hmac, hashlib, base64, sys
repo, remote = sys.argv[1], sys.argv[2]
dek = base64.b64decode(json.load(open(f"{repo}/.omemfs/config"))["remotes"]["origin"]["encryption"]["dek"])
n = hmac.new(dek, b"omemfs:index-root:v1", hashlib.sha256).hexdigest()
print(f"{remote}/objects/{n[0:2]}/{n[2:4]}/{n[4:6]}/{n[6:]}")
PY
}

# ---------------------------------------------------------------------------
# 1. Encrypted remote hides the index root at a derived key
# ---------------------------------------------------------------------------

@test "index_root: encrypted remote stores root at derived key, not INDEX_ROOT" {
    "$OMEMFS" clone --new --url "$REMOTE_DIR" --encrypt repo
    cd repo
    echo "secret" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # The fixed INDEX_ROOT name must NOT exist on an encrypted remote.
    [ ! -f "$REMOTE_DIR/INDEX_ROOT" ]

    # The derived object must exist.
    local derived
    derived="$(derived_index_root_path repo)"
    [ -f "$derived" ]
}

# ---------------------------------------------------------------------------
# 2. Unencrypted remote keeps the fixed INDEX_ROOT name
# ---------------------------------------------------------------------------

@test "index_root: unencrypted remote keeps fixed INDEX_ROOT name" {
    "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    cd repo
    echo "plain" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    [ -f "$REMOTE_DIR/INDEX_ROOT" ]
}

# ---------------------------------------------------------------------------
# 3. Wrong DEK on an existing encrypted clone (via corrupted config)
# ---------------------------------------------------------------------------

@test "index_root: wrong DEK makes the derived index root unreachable (pull guard)" {
    # Create an encrypted repo and push content.
    "$OMEMFS" clone --new --url "$REMOTE_DIR" --encrypt repo
    cd repo
    echo "data" > file.txt
    "$OMEMFS" push
    cd ..

    # Clone via the connection string (the only non-interactive way to obtain
    # the DEK for an existing encrypted remote), then corrupt the DEK so the
    # derived index root key no longer matches the stored object.
    local CONN_STR
    CONN_STR="$( (cd repo && "$OMEMFS" config export) 2>/dev/null | grep '^omemfs_repo_')"
    "$OMEMFS" clone --url "$CONN_STR" cloneB

    # Corrupt the DEK in cloneB to a different valid 32-byte base64 value.
    python - cloneB <<'PY'
import json, base64, sys
p = f"{sys.argv[1]}/.omemfs/config"
cfg = json.load(open(p))
cfg["remotes"]["origin"]["encryption"]["dek"] = base64.b64encode(b"\x01" * 32).decode()
json.dump(cfg, open(p, "w"), indent=2)
PY

    # A pull from cloneB now derives a different (absent) index-root key. Since
    # cloneB has sync history (clone_root from the clone), the post-clone guard
    # must fail rather than silently treat the remote as empty.
    cd cloneB
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [[ "$output" =~ "index root not found on remote" ]]
}

# ---------------------------------------------------------------------------
# 4. --new on a non-empty remote
# ---------------------------------------------------------------------------

@test "index_root: --new on a non-empty remote fails" {
    "$OMEMFS" clone --new --url "$REMOTE_DIR" first
    (cd first && echo x > a.txt && "$OMEMFS" push)

    run "$OMEMFS" clone --new --url "$REMOTE_DIR" second
    [ "$status" -ne 0 ]
    [[ "$output" =~ "remote prefix is not empty" ]]
}

# ---------------------------------------------------------------------------
# 5. Non-TTY without intent flags
# ---------------------------------------------------------------------------

@test "index_root: non-TTY clone without --new/--existing errors" {
    run "$OMEMFS" clone --url "$REMOTE_DIR" repo
    [ "$status" -ne 0 ]
    [[ "$output" =~ "cannot determine remote intent in non-interactive mode" ]]
}

# ---------------------------------------------------------------------------
# 6. Mutually exclusive / incompatible flags
# ---------------------------------------------------------------------------

@test "index_root: --new and --existing together is an error" {
    run "$OMEMFS" clone --new --existing --url "$REMOTE_DIR" repo
    [ "$status" -ne 0 ]
    [[ "$output" =~ "mutually exclusive" ]]
}

@test "index_root: --encrypt with --existing is an error" {
    run "$OMEMFS" clone --existing --encrypt --url "$REMOTE_DIR" repo
    [ "$status" -ne 0 ]
    [[ "$output" =~ "--encrypt is only valid for a new repository" ]]
}

@test "index_root: --existing on an empty unencrypted remote reports INDEX_ROOT missing" {
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" repo
    [ "$status" -ne 0 ]
    [[ "$output" =~ "INDEX_ROOT not found on remote" ]]
}

# ---------------------------------------------------------------------------
# 7. Post-clone sync guard: index root deleted after content was synced
# ---------------------------------------------------------------------------

@test "index_root: guard fails push/pull when INDEX_ROOT deleted (unencrypted)" {
    "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    cd repo
    echo v1 > file.txt
    "$OMEMFS" push

    # Simulate a reset/wrong-remote: remove the index root after syncing.
    rm -f "$REMOTE_DIR/INDEX_ROOT"

    echo v2 > file.txt
    run "$OMEMFS" push
    [ "$status" -ne 0 ]
    [[ "$output" =~ "index root not found on remote" ]]

    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [[ "$output" =~ "index root not found on remote" ]]
}

@test "index_root: guard fails pull when derived root deleted (encrypted)" {
    "$OMEMFS" clone --new --url "$REMOTE_DIR" --encrypt repo
    cd repo
    echo v1 > file.txt
    "$OMEMFS" push
    cd ..

    local derived
    derived="$(derived_index_root_path repo)"
    [ -f "$derived" ]
    rm -f "$derived"

    cd repo
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [[ "$output" =~ "index root not found on remote" ]]
}

# ---------------------------------------------------------------------------
# 8. Guard negative: fresh clone, never pushed → first push succeeds
# ---------------------------------------------------------------------------

@test "index_root: first push on a fresh new clone creates the index root" {
    "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    cd repo
    echo hi > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    [ -f "$REMOTE_DIR/INDEX_ROOT" ]
}

# ---------------------------------------------------------------------------
# 9. config add-backup with --new flags (non-interactive)
# ---------------------------------------------------------------------------

@test "index_root: config add-backup --new --url configures a backup remote" {
    "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    cd repo
    local BACKUP_DIR
    BACKUP_DIR="$(mktemp -d)"
    run "$OMEMFS" config add-backup --new --url "$BACKUP_DIR"
    [ "$status" -eq 0 ]
    [[ "$output" =~ "Backup remote configured." ]]
    # The backup remote must be recorded in config.
    python -c "import json; assert 'backup' in json.load(open('.omemfs/config'))['remotes']"
    rm -rf "$BACKUP_DIR"
}

@test "index_root: config add-backup --new --existing is an error" {
    "$OMEMFS" clone --new --url "$REMOTE_DIR" repo
    cd repo
    run "$OMEMFS" config add-backup --new --existing --url /tmp/whatever
    [ "$status" -ne 0 ]
    [[ "$output" =~ "mutually exclusive" ]]
}
