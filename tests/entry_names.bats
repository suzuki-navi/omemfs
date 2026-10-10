#!/usr/bin/env bats
# Tests for tree-entry names containing a literal backslash.
#
# `\` is an ordinary Linux filename byte, not a path separator (design/01
# "Invariants" #5): `validate_entry_name` in src/object.rs must accept it, and
# no path-string normalisation anywhere in the codebase may rewrite it into a
# `/`, which would silently turn one entry name into a multi-component path
# (e.g. "a\bslash.txt" becoming the two-component path "a/bslash.txt").

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

@test "push+ls: top-level filename with a backslash is listed intact, not split into a subdirectory" {
    printf 'top-level content' > 'a\bslash.txt'
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    [[ "$output" == *'a\bslash.txt'* ]]

    # Must remain a single top-level file, never a directory "a" containing
    # a file "bslash.txt".
    [ -f 'a\bslash.txt' ]
    [ ! -d 'a' ]
    [ ! -e 'a/bslash.txt' ]
}

@test "push+ls+restore+clone: nested filename with backslashes stays one entry, not sub/we/ird/name" {
    mkdir -p sub
    printf 'nested content' > 'sub/we\ird\name'
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # --- ls -r: the entry is listed intact, with both backslashes preserved.
    # (plain `ls` only shows "sub/" as an aggregate row; `-r` descends into it.)
    run "$OMEMFS" ls -r
    [ "$status" -eq 0 ]
    [[ "$output" == *'sub/we\ird\name'* ]]

    # None of the possible partial/full splits on the backslashes exist.
    [ ! -e 'sub/we/ird/name' ]
    [ ! -e 'sub/we/ird\name' ]
    [ ! -e 'sub/we\ird/name' ]
    [ -f 'sub/we\ird\name' ]

    # --- restore: delete locally, restore, and confirm exact content back. ---
    rm 'sub/we\ird\name'
    run "$OMEMFS" restore 'sub/we\ird\name'
    [ "$status" -eq 0 ]
    [ -f 'sub/we\ird\name' ]
    [ ! -e 'sub/we/ird/name' ]
    [ "$(cat 'sub/we\ird\name')" = "nested content" ]

    # Round-trip via `cat` against the clone root, independent of the
    # materialised working-tree file.
    run "$OMEMFS" cat 'clone-root:sub/we\ird\name'
    [ "$status" -eq 0 ]
    [ "$output" = "nested content" ]

    # --- clone: a brand-new clone from the same remote materialises the
    # same single file, never the split sub/we/ird/name.
    local dest
    dest="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$dest/verify"
    [ "$status" -eq 0 ]
    [ -f "$dest/verify/sub/we\\ird\\name" ]
    [ ! -e "$dest/verify/sub/we/ird/name" ]
    [ ! -e "$dest/verify/sub/we/ird\\name" ]
    [ ! -e "$dest/verify/sub/we\\ird/name" ]
    [ "$(cat "$dest/verify/sub/we\\ird\\name")" = "nested content" ]

    rm -rf "$dest"
}

@test "push+cat: top-level backslash filename content round-trips byte-exact" {
    printf 'exact top content' > 'a\bslash.txt'
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" cat 'clone-root:a\bslash.txt'
    [ "$status" -eq 0 ]
    [ "$output" = "exact top content" ]

    rm 'a\bslash.txt'
    run "$OMEMFS" restore 'a\bslash.txt'
    [ "$status" -eq 0 ]
    [ "$(cat 'a\bslash.txt')" = "exact top content" ]
}
