#!/usr/bin/env bats
# Integration tests for the S3 backend against MinIO.
#
# Env-gated: skipped unless OMEMFS_S3_TEST_ENDPOINT is set (see
# design/13_cloud_backends.md, "Testing strategy"). To run, start MinIO and
# create the bucket, e.g.:
#
#   docker run -d -p 9000:9000 -e MINIO_ROOT_USER=minioadmin \
#       -e MINIO_ROOT_PASSWORD=minioadmin quay.io/minio/minio server /data
#   AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin \
#       aws --endpoint-url http://localhost:9000 s3 mb s3://omemfs-test
#   OMEMFS_S3_TEST_ENDPOINT=http://localhost:9000 \
#       OMEMFS="$(pwd)/target/debug/omemfs" bats tests/s3.bats
#
# Behavioral assertions only — no peeking at remote storage internals.

load test_helper/common

setup() {
    require_s3
    setup_test_dir
}

teardown() {
    teardown_test_dir
}

# Bootstrap an empty S3-backed repo named "$1" under a fresh unique prefix.
s3_new_repo() {
    seed_repo_config "$1" "$(s3_config_json "$S3_PREFIX")"
}

# ---------------------------------------------------------------------------
# clone (new / existing)
#
# NOTE: these clones go through a connection string (`config export` →
# `clone --url omemfs_repo_...`), which is the only non-interactive way to carry
# a custom `endpoint` (a plain `s3://` URL cannot). The connection-string path
# imports the config verbatim and intentionally bypasses
# `validate_remote_against_intent` (`--new`/`--existing` are ignored for a
# connection string — see src/commands/clone.rs). The new/existing validation
# gate itself is covered by unit tests in src/commands/clone.rs over the Local +
# MemCloud backends, so it is not re-asserted here.
# ---------------------------------------------------------------------------

@test "s3: clone --new into an empty prefix succeeds" {
    s3_new_repo origin_seed
    local conn
    conn="$(repo_connection_string origin_seed)"
    run "$OMEMFS" clone --url "$conn" --new fresh
    [ "$status" -eq 0 ]
    [ -d fresh/.omemfs ]
}

@test "s3: clone after a push downloads content" {
    s3_new_repo seed
    echo "hello cloud" > seed/greeting.txt
    push_repo seed
    local conn
    conn="$(repo_connection_string seed)"
    run "$OMEMFS" clone --url "$conn" --existing dl
    [ "$status" -eq 0 ]
    [ "$(cat dl/greeting.txt)" = "hello cloud" ]
}

# ---------------------------------------------------------------------------
# push (full / scoped / delete)
# ---------------------------------------------------------------------------

@test "s3: push uploads files and reports a remote root" {
    s3_new_repo repo
    echo "one" > repo/one.txt
    echo "two" > repo/two.txt
    run bash -c "cd repo && '$OMEMFS' push"
    [ "$status" -eq 0 ]
    [[ "$output" == *"Remote root:"* ]]
}

@test "s3: second push with no changes reports nothing to push" {
    s3_new_repo repo
    echo "one" > repo/one.txt
    push_repo repo
    run bash -c "cd repo && '$OMEMFS' push"
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}

@test "s3: path-scoped push uploads only the named subtree" {
    s3_new_repo repo
    mkdir -p repo/src repo/doc
    echo "code" > repo/src/main.txt
    echo "docs" > repo/doc/readme.txt
    run bash -c "cd repo && '$OMEMFS' push src"
    [ "$status" -eq 0 ]
    # A subsequent full push still finds the doc subtree to upload.
    run bash -c "cd repo && '$OMEMFS' push"
    [ "$status" -eq 0 ]
    [[ "${output,,}" != *"nothing to push"* ]]
}

@test "s3: deleting a file then pushing propagates the deletion" {
    s3_new_repo repo
    echo "keep" > repo/keep.txt
    echo "gone" > repo/gone.txt
    push_repo repo
    rm repo/gone.txt
    push_repo repo
    # A fresh clone must not resurrect the deleted file.
    local conn
    conn="$(repo_connection_string repo)"
    run "$OMEMFS" clone --url "$conn" --existing verify
    [ "$status" -eq 0 ]
    [ -f verify/keep.txt ]
    [ ! -f verify/gone.txt ]
}

# ---------------------------------------------------------------------------
# pull (full / scoped / conflict)
# ---------------------------------------------------------------------------

@test "s3: pull brings a sibling clone up to date" {
    s3_new_repo a
    echo "v1" > a/shared.txt
    push_repo a
    local conn
    conn="$(repo_connection_string a)"
    run "$OMEMFS" clone --url "$conn" --existing b
    [ "$status" -eq 0 ]
    # a pushes a new file; b pulls it.
    echo "from a" > a/newfile.txt
    push_repo a
    run bash -c "cd b && '$OMEMFS' pull"
    [ "$status" -eq 0 ]
    [ "$(cat b/newfile.txt)" = "from a" ]
}

@test "s3: concurrent push — exactly one wins, loser hits CAS error" {
    s3_new_repo seed
    echo "base" > seed/base.txt
    push_repo seed
    local conn
    conn="$(repo_connection_string seed)"
    run "$OMEMFS" clone --url "$conn" --existing cA
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --url "$conn" --existing cB
    [ "$status" -eq 0 ]

    echo "from A" > cA/a.txt
    echo "from B" > cB/b.txt
    local base="$PWD"
    rm -f "$base/a.status" "$base/b.status"
    bash -c "cd '$base/cA' && '$OMEMFS' push > '$base/a.out' 2>&1; echo \$? > '$base/a.status'" 0</dev/null &
    bash -c "cd '$base/cB' && '$OMEMFS' push > '$base/b.out' 2>&1; echo \$? > '$base/b.status'" 0</dev/null &
    wait

    local sa sb
    sa="$(cat "$base/a.status")"
    sb="$(cat "$base/b.status")"
    # Exactly one succeeds (0) and one fails (non-zero).
    [ "$sa" != "$sb" ]
    [ "$sa" -eq 0 ] || [ "$sb" -eq 0 ]
    # The loser's message names the CAS / out-of-date condition.
    if [ "$sa" -ne 0 ]; then
        [[ "$(cat "$base/a.out")" == *"updated since last sync"* ]]
    else
        [[ "$(cat "$base/b.out")" == *"updated since last sync"* ]]
    fi
}

# ---------------------------------------------------------------------------
# cat / stats / expand / pack
# ---------------------------------------------------------------------------

@test "s3: cat reads a blob through the remote pack reader" {
    s3_new_repo seed
    echo "catme" > seed/catme.txt
    push_repo seed
    local conn
    conn="$(repo_connection_string seed)"
    run "$OMEMFS" clone --url "$conn" --existing reader
    [ "$status" -eq 0 ]
    # Resolve the blob hash from the tree, then cat it through the remote.
    local hash
    hash="$(cd reader && "$OMEMFS" ls --full-hash catme.txt | awk '{print $1}')"
    [ -n "$hash" ]
    run bash -c "cd reader && '$OMEMFS' cat '$hash'"
    [ "$status" -eq 0 ]
    [ "$output" = "catme" ]
}

@test "s3: stats --remote classifies remote objects" {
    s3_new_repo repo
    echo "stat me" > repo/s.txt
    push_repo repo
    run bash -c "cd repo && '$OMEMFS' stats --remote"
    [ "$status" -eq 0 ]
    # The remote-backed section must actually be produced for a cloud remote,
    # not silently skipped. Regression guard: stats --remote previously only
    # wired the Local backend, so cloud remotes exited 0 with the section absent.
    [[ "$output" == *"Remote storage  (origin)"* ]]
    [[ "$output" == *"index-root"* ]]
    [[ "$output" == *"Remote object sizes"* ]]
}

@test "s3: stats --remote --json emits remote_storage for a cloud remote" {
    s3_new_repo repo
    echo "stat me" > repo/s.txt
    push_repo repo
    run bash -c "cd repo && '$OMEMFS' stats --remote --json"
    [ "$status" -eq 0 ]
    [[ "$output" == *'"remote_storage"'* ]]
    [[ "$output" == *'"remote_object_histogram"'* ]]
}

@test "s3: expand materialises a stubbed file from the remote" {
    s3_new_repo seed
    head -c 200000 /dev/zero | tr '\0' 'x' > seed/big.txt
    push_repo seed
    local conn
    conn="$(repo_connection_string seed)"
    # Clone with a low stub threshold so big.txt is stubbed (not downloaded).
    run "$OMEMFS" clone --url "$conn" --existing --stub-threshold 1024 exp
    [ "$status" -eq 0 ]
    run bash -c "cd exp && '$OMEMFS' expand big.txt"
    [ "$status" -eq 0 ]
    [ "$(wc -c < exp/big.txt)" -eq 200000 ]
}

@test "s3: pack runs against the remote without error" {
    s3_new_repo repo
    echo "packme" > repo/p.txt
    push_repo repo
    run bash -c "cd repo && '$OMEMFS' pack"
    [ "$status" -eq 0 ]
}
