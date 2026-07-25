#!/usr/bin/env bats
# Integration tests for the GCS backend.
#
# Env-gated: skipped unless OMEMFS_GCS_TEST_ENDPOINT is set (see
# design/13_cloud_backends.md, "Testing strategy"). Behavioral assertions only;
# the adapter connects anonymously when no credentials are configured.
#
# IMPORTANT — emulator transport limitation. The `google-cloud-storage` client
# is HYBRID: the data plane (`write_object`/`read_object`) speaks REST while the
# control plane (`get_object` metadata) speaks gRPC. Real GCS serves both on one
# host (`storage.googleapis.com:443`), so omemfs's single `endpoint` config is
# correct in production. The Google storage-testbench, however, serves REST and
# gRPC on SEPARATE ports, so no single `endpoint` value satisfies both planes —
# the testbench cannot exercise the full push/pull path. (This was confirmed
# live: with the REST port, `get_object` fails the gRPC handshake; with the gRPC
# port, `write_object` fails the REST handshake.) The gRPC-vs-HTTP status
# detection that this surfaced is regression-locked by unit tests in
# `src/store/cloud/gcs.rs` (`has_status_matches_equivalent_grpc_codes`).
#
# Therefore OMEMFS_GCS_TEST_ENDPOINT is intended for a REAL GCS bucket (or a
# future single-endpoint emulator). To run against real GCS, point the endpoint
# at the GCS host and supply credentials via the config (see common.bash).
#   OMEMFS_GCS_TEST_ENDPOINT=https://storage.googleapis.com \
#       OMEMFS="$(pwd)/target/debug/omemfs" bats tests/gcs.bats

load test_helper/common

setup() {
    require_gcs
    setup_test_dir
}

teardown() {
    teardown_test_dir
}

gcs_new_repo() {
    seed_repo_config "$1" "$(gcs_config_json "$GCS_PREFIX")"
}

# ---------------------------------------------------------------------------
# clone (see the note in tests/s3.bats: clones go through a connection string,
# which bypasses the new/existing validation gate by design — that gate is
# unit-tested in src/commands/clone.rs over Local + MemCloud).
# ---------------------------------------------------------------------------

@test "gcs: clone --new into an empty prefix succeeds" {
    gcs_new_repo seed
    local conn
    conn="$(repo_connection_string seed)"
    run "$OMEMFS" clone --url "$conn" --new fresh
    [ "$status" -eq 0 ]
    [ -d fresh/.omemfs ]
}

@test "gcs: clone after a push downloads content" {
    gcs_new_repo seed
    echo "hello gcs" > seed/greeting.txt
    push_repo seed
    local conn
    conn="$(repo_connection_string seed)"
    run "$OMEMFS" clone --url "$conn" --existing dl
    [ "$status" -eq 0 ]
    [ "$(cat dl/greeting.txt)" = "hello gcs" ]
}

# ---------------------------------------------------------------------------
# push (full / scoped / delete)
# ---------------------------------------------------------------------------

@test "gcs: push uploads files and reports a remote root" {
    gcs_new_repo repo
    echo "one" > repo/one.txt
    echo "two" > repo/two.txt
    run bash -c "cd repo && '$OMEMFS' push"
    [ "$status" -eq 0 ]
    [[ "$output" == *"Remote root:"* ]]
}

@test "gcs: second push with no changes reports nothing to push" {
    gcs_new_repo repo
    echo "one" > repo/one.txt
    push_repo repo
    run bash -c "cd repo && '$OMEMFS' push"
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}

@test "gcs: path-scoped push uploads only the named subtree" {
    gcs_new_repo repo
    mkdir -p repo/src repo/doc
    echo "code" > repo/src/main.txt
    echo "docs" > repo/doc/readme.txt
    run bash -c "cd repo && '$OMEMFS' push src"
    [ "$status" -eq 0 ]
    run bash -c "cd repo && '$OMEMFS' push"
    [ "$status" -eq 0 ]
    [[ "${output,,}" != *"nothing to push"* ]]
}

@test "gcs: deleting a file then pushing propagates the deletion" {
    gcs_new_repo repo
    echo "keep" > repo/keep.txt
    echo "gone" > repo/gone.txt
    push_repo repo
    rm repo/gone.txt
    push_repo repo
    local conn
    conn="$(repo_connection_string repo)"
    run "$OMEMFS" clone --url "$conn" --existing verify
    [ "$status" -eq 0 ]
    [ -f verify/keep.txt ]
    [ ! -f verify/gone.txt ]
}

# ---------------------------------------------------------------------------
# pull (full / conflict)
# ---------------------------------------------------------------------------

@test "gcs: pull brings a sibling clone up to date" {
    gcs_new_repo a
    echo "v1" > a/shared.txt
    push_repo a
    local conn
    conn="$(repo_connection_string a)"
    run "$OMEMFS" clone --url "$conn" --existing b
    [ "$status" -eq 0 ]
    echo "from a" > a/newfile.txt
    push_repo a
    run bash -c "cd b && '$OMEMFS' pull"
    [ "$status" -eq 0 ]
    [ "$(cat b/newfile.txt)" = "from a" ]
}

@test "gcs: concurrent push — exactly one wins, loser hits CAS error" {
    # Depends on storage-testbench enforcing ifGenerationMatch (412).
    gcs_new_repo seed
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
    [ "$sa" != "$sb" ]
    [ "$sa" -eq 0 ] || [ "$sb" -eq 0 ]
    if [ "$sa" -ne 0 ]; then
        [[ "$(cat "$base/a.out")" == *"updated since last sync"* ]]
    else
        [[ "$(cat "$base/b.out")" == *"updated since last sync"* ]]
    fi
}

# ---------------------------------------------------------------------------
# cat / stats / expand / pack
# ---------------------------------------------------------------------------

@test "gcs: cat reads a blob through the remote pack reader" {
    gcs_new_repo seed
    echo "catme" > seed/catme.txt
    push_repo seed
    local conn
    conn="$(repo_connection_string seed)"
    run "$OMEMFS" clone --url "$conn" --existing reader
    [ "$status" -eq 0 ]
    local hash
    hash="$(cd reader && "$OMEMFS" ls --full-hash catme.txt | awk '{print $1}')"
    [ -n "$hash" ]
    run bash -c "cd reader && '$OMEMFS' cat '$hash'"
    [ "$status" -eq 0 ]
    [ "$output" = "catme" ]
}

@test "gcs: stats --remote classifies remote objects" {
    gcs_new_repo repo
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

@test "gcs: stats --remote --json emits remote_storage for a cloud remote" {
    gcs_new_repo repo
    echo "stat me" > repo/s.txt
    push_repo repo
    run bash -c "cd repo && '$OMEMFS' stats --remote --json"
    [ "$status" -eq 0 ]
    [[ "$output" == *'"remote_storage"'* ]]
    [[ "$output" == *'"remote_object_histogram"'* ]]
}

@test "gcs: expand materialises a stubbed file from the remote" {
    gcs_new_repo seed
    head -c 200000 /dev/zero | tr '\0' 'x' > seed/big.txt
    push_repo seed
    local conn
    conn="$(repo_connection_string seed)"
    run "$OMEMFS" clone --url "$conn" --existing --stub-threshold 1024 exp
    [ "$status" -eq 0 ]
    run bash -c "cd exp && '$OMEMFS' expand big.txt"
    [ "$status" -eq 0 ]
    [ "$(wc -c < exp/big.txt)" -eq 200000 ]
}

@test "gcs: pack runs against the remote without error" {
    gcs_new_repo repo
    echo "packme" > repo/p.txt
    push_repo repo
    run bash -c "cd repo && '$OMEMFS' pack"
    [ "$status" -eq 0 ]
}
