#!/usr/bin/env bats
# Tests for `omemfs expand` resolving a path nested inside an unexpanded
# directory stub by lowering the stub frontier (design/04_cli_spec.md,
# "Paths below the stub frontier"; design/08_stub_system.md, "Lowering the
# stub frontier past a `.git` entry").

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
# Helper: build a remote with a nested subtree under ai-labo, then clone it
# with a small stub threshold so ai-labo (aggregate size >= threshold) comes
# down as a single unexpanded directory stub. Nothing below ai-labo exists
# on disk in $TEST_DIR/dest afterward.
# ---------------------------------------------------------------------------
seed_nested_stub() {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src
    [ "$status" -eq 0 ]
    mkdir -p src/ai-labo/ja-tochigi/nested
    echo "one" > src/ai-labo/ja-tochigi/a.txt
    echo "two" > src/ai-labo/ja-tochigi/b.txt
    echo "deep" > src/ai-labo/ja-tochigi/nested/deep.txt
    echo "readme" > src/ai-labo/readme.txt
    printf '%0200d' 0 > src/ai-labo/large.bin
    run bash -c "cd src && '$OMEMFS' push"
    [ "$status" -eq 0 ]

    # Clone with a 50-byte threshold: ai-labo (aggregate > 50) is stubbed as
    # a single directory marker.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 50 dest
    [ "$status" -eq 0 ]
    [ -f dest/ai-labo/.omemfs-stub ]
    [ ! -e dest/ai-labo/ja-tochigi ]
}

# ---------------------------------------------------------------------------
# Helper: like seed_nested_stub, but ai-labo is itself a Git working tree
# (a real .git created before push), for the `.git`-during-lowering tests.
# ---------------------------------------------------------------------------
seed_nested_git_stub() {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src
    [ "$status" -eq 0 ]
    mkdir -p src/ai-labo/ja-tochigi
    git -C src/ai-labo init -q
    echo "one" > src/ai-labo/ja-tochigi/a.txt
    printf '%0200d' 0 > src/ai-labo/large.bin
    run bash -c "cd src && '$OMEMFS' push"
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 50 dest
    [ "$status" -eq 0 ]
    [ -f dest/ai-labo/.omemfs-stub ]
    [ ! -e dest/ai-labo/.git ]
}

@test "expand: resolves a single-level path nested inside an unexpanded stub" {
    seed_nested_stub
    cd dest
    run "$OMEMFS" expand ai-labo/ja-tochigi
    [ "$status" -eq 0 ]
    [ ! -f ai-labo/.omemfs-stub ]
    [ -f ai-labo/ja-tochigi/a.txt ]
    [ -f ai-labo/ja-tochigi/b.txt ]
    [ -f ai-labo/ja-tochigi/nested/deep.txt ]
    [ ! -f ai-labo/ja-tochigi/.omemfs-stub ]
    # Unrelated siblings become individual stubs rather than being downloaded.
    [ -f ai-labo/large.bin.omemfs-stub ]
    [ ! -f ai-labo/large.bin ]
    [ -f ai-labo/readme.txt.omemfs-stub ]
    [ ! -f ai-labo/readme.txt ]
}

@test "expand: resolves a multi-level path nested inside an unexpanded stub" {
    seed_nested_stub
    cd dest
    run "$OMEMFS" expand ai-labo/ja-tochigi/nested/deep.txt
    [ "$status" -eq 0 ]
    [ -f ai-labo/ja-tochigi/nested/deep.txt ]
    [ ! -f ai-labo/ja-tochigi/nested/deep.txt.omemfs-stub ]
    # Every level lowered on the way down leaves its other children stubbed.
    [ -f ai-labo/ja-tochigi/a.txt.omemfs-stub ]
    [ ! -f ai-labo/ja-tochigi/a.txt ]
    [ -f ai-labo/ja-tochigi/b.txt.omemfs-stub ]
    [ -f ai-labo/large.bin.omemfs-stub ]
}

@test "expand: -r expands only the target, unrelated siblings stay stubbed" {
    seed_nested_stub
    cd dest
    run "$OMEMFS" expand -r ai-labo/ja-tochigi/nested/deep.txt
    [ "$status" -eq 0 ]
    [ -f ai-labo/ja-tochigi/nested/deep.txt ]
    # readme.txt is tiny -- well under any real threshold -- but it is an
    # unrelated sibling passed over while descending, so -r must not pull it
    # in: -r/--stub-threshold govern depth inside the target, not how much of
    # the ancestor is materialised to reach it.
    [ -f ai-labo/readme.txt.omemfs-stub ]
    [ ! -f ai-labo/readme.txt ]
}

@test "expand: multiple targets sharing an ancestor both resolve in one call" {
    seed_nested_stub
    cd dest
    run "$OMEMFS" expand ai-labo/ja-tochigi/a.txt ai-labo/ja-tochigi/b.txt
    [ "$status" -eq 0 ]
    [ -f ai-labo/ja-tochigi/a.txt ]
    [ -f ai-labo/ja-tochigi/b.txt ]
    # "nested" is a sibling of both targets at the ja-tochigi level -- stubbed.
    [ -f ai-labo/ja-tochigi/nested/.omemfs-stub ]
    [ -f ai-labo/large.bin.omemfs-stub ]
}

@test "expand: --dry-run on a nested path reads but never writes the cache" {
    seed_nested_stub
    cd dest
    before=$(find .omemfs/objects -type f | wc -l)
    run "$OMEMFS" expand --dry-run ai-labo/ja-tochigi/a.txt
    [ "$status" -eq 0 ]
    [[ "$output" == *"would expand: ai-labo/ja-tochigi/a.txt"* ]]
    # Nothing on disk changed: ai-labo is still exactly a single-marker stub.
    [ -f ai-labo/.omemfs-stub ]
    [ ! -e ai-labo/ja-tochigi ]
    after=$(find .omemfs/objects -type f | wc -l)
    [ "$before" -eq "$after" ]
}

@test "expand: errors on a nonexistent path nested inside a stub" {
    seed_nested_stub
    cd dest
    run "$OMEMFS" expand ai-labo/no-such-dir
    [ "$status" -ne 0 ]
    [[ "$output" == *"does not exist"* ]]
    # No partial state left behind by the failed attempt.
    [ -f ai-labo/.omemfs-stub ] || [ -f ai-labo/large.bin.omemfs-stub ]
}

@test "expand: errors when a mid-path component is a file, not a directory" {
    seed_nested_stub
    cd dest
    run "$OMEMFS" expand ai-labo/readme.txt/inner
    [ "$status" -ne 0 ]
    [[ "$output" == *"is a file, not a directory"* ]]
}

@test "expand: lowers past a .git sibling, .git becomes its own atomic stub" {
    seed_nested_git_stub
    cd dest
    run "$OMEMFS" expand ai-labo/ja-tochigi/a.txt
    [ "$status" -eq 0 ]
    [ -f ai-labo/ja-tochigi/a.txt ]
    [ -f ai-labo/.git/.omemfs-stub ]
    # .git must never be partially expanded.
    [ ! -f ai-labo/.git/config ]
    [ -f ai-labo/large.bin.omemfs-stub ]
}

@test "expand: refuses a target on or below a stubbed .git directory" {
    seed_nested_git_stub
    cd dest
    run "$OMEMFS" expand ai-labo/.git/config
    [ "$status" -ne 0 ]
    [[ "$output" == *"is a stubbed Git directory"* ]]
    # ai-labo's level is still safely lowered -- .git became its own atomic
    # stub -- the refusal only blocks descending further into it.
    [ -f ai-labo/.git/.omemfs-stub ]
    [ ! -f ai-labo/.git/config ]
}
