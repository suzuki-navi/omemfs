#!/usr/bin/env bats
# Integration tests for the Azure Blob backend.
#
# Env-gated and OPT-IN: skipped unless all OMEMFS_AZURE_TEST_* vars are set
# (see design/13_cloud_backends.md, "Testing strategy"). Azure has NO local
# emulator in this design — omemfs authenticates with Entra ID only, and
# Azurite supports only shared-key / SAS — so these tests require a REAL Azure
# Storage account plus a service principal (client_id / client_secret /
# tenant_id) with Blob Data Contributor on the container. The shared in-memory
# MemCloud fake carries the Azure CAS coverage in the default unit suite.
#
# To run:
#   OMEMFS_AZURE_TEST_ACCOUNT=<account> \
#   OMEMFS_AZURE_TEST_CONTAINER=<container> \
#   OMEMFS_AZURE_TEST_TENANT_ID=<tenant> \
#   OMEMFS_AZURE_TEST_CLIENT_ID=<app-id> \
#   OMEMFS_AZURE_TEST_CLIENT_SECRET=<secret> \
#   OMEMFS="$(pwd)/target/debug/omemfs" bats tests/azure.bats
#
# Behavioral assertions only.

load test_helper/common

setup() {
    require_azure
    setup_test_dir
}

teardown() {
    teardown_test_dir
}

azure_new_repo() {
    seed_repo_config "$1" "$(azure_config_json "$AZ_PREFIX")"
}

# ---------------------------------------------------------------------------
# clone (see the note in tests/s3.bats: clones go through a connection string,
# which bypasses the new/existing validation gate by design).
# ---------------------------------------------------------------------------

@test "azure: clone --new into an empty prefix succeeds" {
    azure_new_repo seed
    local conn
    conn="$(repo_connection_string seed)"
    run "$OMEMFS" clone --url "$conn" --new fresh
    [ "$status" -eq 0 ]
    [ -d fresh/.omemfs ]
}

@test "azure: clone after a push downloads content" {
    azure_new_repo seed
    echo "hello azure" > seed/greeting.txt
    push_repo seed
    local conn
    conn="$(repo_connection_string seed)"
    run "$OMEMFS" clone --url "$conn" --existing dl
    [ "$status" -eq 0 ]
    [ "$(cat dl/greeting.txt)" = "hello azure" ]
}

# ---------------------------------------------------------------------------
# push / pull
# ---------------------------------------------------------------------------

@test "azure: push uploads files and reports a remote root" {
    azure_new_repo repo
    echo "one" > repo/one.txt
    echo "two" > repo/two.txt
    run bash -c "cd repo && '$OMEMFS' push"
    [ "$status" -eq 0 ]
    [[ "$output" == *"Remote root:"* ]]
}

@test "azure: second push with no changes reports nothing to push" {
    azure_new_repo repo
    echo "one" > repo/one.txt
    push_repo repo
    run bash -c "cd repo && '$OMEMFS' push"
    [ "$status" -eq 0 ]
    [[ "${output,,}" == *"nothing to push"* ]]
}

@test "azure: deleting a file then pushing propagates the deletion" {
    azure_new_repo repo
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

@test "azure: pull brings a sibling clone up to date" {
    azure_new_repo a
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

@test "azure: concurrent push — exactly one wins, loser hits CAS error" {
    azure_new_repo seed
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

@test "azure: cat reads a blob through the remote pack reader" {
    azure_new_repo seed
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

@test "azure: stats --remote classifies remote objects" {
    azure_new_repo repo
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

@test "azure: stats --remote --json emits remote_storage for a cloud remote" {
    azure_new_repo repo
    echo "stat me" > repo/s.txt
    push_repo repo
    run bash -c "cd repo && '$OMEMFS' stats --remote --json"
    [ "$status" -eq 0 ]
    [[ "$output" == *'"remote_storage"'* ]]
    [[ "$output" == *'"remote_object_histogram"'* ]]
}

@test "azure: expand materialises a stubbed file from the remote" {
    azure_new_repo seed
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

@test "azure: pack runs against the remote without error" {
    azure_new_repo repo
    echo "packme" > repo/p.txt
    push_repo repo
    run bash -c "cd repo && '$OMEMFS' pack"
    [ "$status" -eq 0 ]
}
