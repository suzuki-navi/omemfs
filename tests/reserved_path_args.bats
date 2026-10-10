#!/usr/bin/env bats
# A reserved omemfs name (design/09 "Reserved names as command arguments",
# design/04 "Repository discovery") is refused as a <path> argument by every
# path-taking command, with the same message, before anything is touched.
#
# The motivating bug: `push <reserved>` used to upload the name, planting it in
# the remote tree, after which every later `push` and every `clone` of that
# remote failed on the untrusted-tree check.

load test_helper/common

setup() {
    setup_repo
    echo "hello" > a.txt
    mkdir d
    echo "x" > d/f.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# Assert that running `omemfs <args...>` fails with the reserved-name message.
assert_reserved_refusal() {
    local name="$1"
    shift
    run "$OMEMFS" "$@"
    [ "$status" -eq 1 ]
    [[ "$output" == *"reserved omemfs name"* ]]
    [[ "$output" == *"$name"* ]]
}

@test "reserved path: every path-taking command refuses a file-adjacent stub marker" {
    echo "hand made" > foo.omemfs-stub
    for cmd in push pull ls restore expand stub; do
        assert_reserved_refusal foo.omemfs-stub "$cmd" foo.omemfs-stub
    done
    # Nothing was touched.
    [ "$(cat foo.omemfs-stub)" = "hand made" ]
}

@test "reserved path: every path-taking command refuses a conflict helper name" {
    echo "hand made" > a.txt.omemfs-conflict-base
    for cmd in push pull ls restore expand stub; do
        assert_reserved_refusal a.txt.omemfs-conflict-base "$cmd" a.txt.omemfs-conflict-base
    done
    [ -f a.txt.omemfs-conflict-base ]
}

@test "reserved path: an unknown .omemfs- name from a newer version is refused too" {
    echo "zzz" > .omemfs-newthing
    for cmd in push pull ls restore expand stub; do
        assert_reserved_refusal .omemfs-newthing "$cmd" .omemfs-newthing
    done
}

@test "reserved path: the .omemfs metadata directory, and anything below it, is refused" {
    for cmd in push pull ls restore expand stub; do
        assert_reserved_refusal .omemfs "$cmd" .omemfs
        assert_reserved_refusal .omemfs/config "$cmd" .omemfs/config
    done
}

@test "reserved path: a reserved component in the middle of a path is refused" {
    mkdir -p d/.omemfs-stub
    assert_reserved_refusal d/.omemfs-stub/x push d/.omemfs-stub/x
    assert_reserved_refusal d/.omemfs-stub/x restore d/.omemfs-stub/x
}

@test "reserved path: '..' and absolute spellings cannot route around the rule" {
    echo "zzz" > .omemfs-newthing
    mkdir -p d/inner
    cd d/inner
    assert_reserved_refusal .omemfs-newthing push ../../.omemfs-newthing
    assert_reserved_refusal .omemfs-newthing restore "$(cd ../.. && pwd)/.omemfs-newthing"
}

@test "reserved path: push of a reserved name no longer poisons the remote" {
    echo "zzz" > .omemfs-newthing
    run "$OMEMFS" push .omemfs-newthing
    [ "$status" -eq 1 ]
    # The repository and the remote are still fully usable afterwards.
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$BATS_TEST_TMPDIR/second"
    [ "$status" -eq 0 ]
    [ -f "$BATS_TEST_TMPDIR/second/a.txt" ]
}

@test "reserved path: restore of a reserved name no longer deletes it" {
    echo "keep me" > foo.omemfs-stub
    run "$OMEMFS" restore foo.omemfs-stub
    [ "$status" -eq 1 ]
    [ "$(cat foo.omemfs-stub)" = "keep me" ]
}

@test "reserved path: .omemfs-filter is still an ordinary addressable file" {
    run "$OMEMFS" ls .omemfs-filter
    [ "$status" -eq 0 ]
    [[ "$output" == *".omemfs-filter"* ]]
    run "$OMEMFS" push .omemfs-filter
    [ "$status" -eq 0 ]
    run "$OMEMFS" restore .omemfs-filter
    [ "$status" -eq 0 ]
}

@test "reserved path: ls still LISTS a reserved file; only naming it as an argument is refused" {
    echo "zzz" > .omemfs-newthing
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    [[ "$output" == *".omemfs-newthing"* ]]
}

@test "reserved path: a reserved-looking path outside the repository keeps the outside-the-repository error" {
    run "$OMEMFS" ls /tmp/elsewhere/.omemfs
    [ "$status" -ne 0 ]
    [[ "$output" == *"outside the repository"* ]]
}

@test "reserved path: conflict commands still accept a search scope that passes near reserved names" {
    run "$OMEMFS" conflict list
    [ "$status" -eq 0 ]
    run "$OMEMFS" conflict list d
    [ "$status" -eq 0 ]
}
