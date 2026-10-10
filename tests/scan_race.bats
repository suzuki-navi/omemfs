#!/usr/bin/env bats
# The working-tree scan reads through directory fds, so a name that is replaced
# after it was listed cannot redirect the read (design/03_sync_model.md, "The
# read side: the working-tree scan and content capture").
#
# The races are made deterministic with two debug-build hooks:
#   OMEMFS_TEST_SCAN_AFTER_LIST_DELAY_MS   sleep after a directory is listed
#   OMEMFS_TEST_SCAN_AFTER_LIST_MARKER     create this file right after the first
#                                          listing, so the swapper knows the
#                                          listing is over

load test_helper/common

setup() {
    setup_repo
    OUT="$(mktemp -d)"
    MARKER="$(mktemp -u)"
    printf 'SECRET-OUTSIDE\n' > "$OUT/secret.txt"
}

teardown() {
    rm -rf "$OUT" "$MARKER"
    teardown_local_remote
    teardown_test_dir
}

# Run `omemfs push [args]` with the scan paused after its first listing, and run
# the shell function $SWAP in between. Sets $status/$output like `run`.
push_with_swap() {
    ( until [ -e "$MARKER" ]; do sleep 0.01; done; $SWAP ) &
    local swapper=$!
    run env OMEMFS_TEST_SCAN_AFTER_LIST_DELAY_MS=600 \
        OMEMFS_TEST_SCAN_AFTER_LIST_MARKER="$MARKER" \
        timeout 60 "$OMEMFS" push "$@"
    wait "$swapper"
}

# Clone the remote into a fresh directory and pull; leaves cwd unchanged.
verify_remote() {
    VERIFY="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$VERIFY/v"
    [ "$status" -eq 0 ]
    ( cd "$VERIFY/v" && "$OMEMFS" pull >/dev/null )
}

swap_dir_for_symlink() { rm -rf d; ln -s "$OUT" d; }
swap_file_for_symlink() { rm f.txt; ln -s "$OUT/secret.txt" f.txt; }
swap_file_for_fifo() { rm f.txt; mkfifo f.txt; }
swap_subdir_for_symlink() { rm -rf d/sub; ln -s "$OUT" d/sub; }

@test "scan race: a directory replaced by a symlink after listing is not followed (full push)" {
    mkdir d
    echo inside > d/a.txt
    echo stable-old > stable.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo stable-new > stable.txt
    SWAP=swap_dir_for_symlink
    push_with_swap

    [ "$status" -eq 0 ]
    [[ "$output" == *"actively changing path"* ]]

    verify_remote
    [ "$(cat "$VERIFY/v/d/a.txt")" = "inside" ]
    [ ! -e "$VERIFY/v/d/secret.txt" ]
    [ "$(cat "$VERIFY/v/stable.txt")" = "stable-new" ]
    rm -rf "$VERIFY"
}

@test "scan race: a subdirectory replaced by a symlink after listing is not followed (scoped push)" {
    mkdir -p d/sub
    echo inside > d/sub/a.txt
    echo top-old > d/top.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo top-new > d/top.txt
    SWAP=swap_subdir_for_symlink
    push_with_swap d

    [ "$status" -eq 0 ]
    [[ "$output" == *"actively changing path"* ]]

    verify_remote
    [ "$(cat "$VERIFY/v/d/sub/a.txt")" = "inside" ]
    [ ! -e "$VERIFY/v/d/sub/secret.txt" ]
    [ "$(cat "$VERIFY/v/d/top.txt")" = "top-new" ]
    rm -rf "$VERIFY"
}

@test "scan race: a file replaced by a symlink after listing is not read through the link" {
    echo old > f.txt
    echo stable-old > stable.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo stable-new > stable.txt
    SWAP=swap_file_for_symlink
    push_with_swap

    [ "$status" -eq 0 ]
    [[ "$output" == *"actively changing path"* ]]

    verify_remote
    [ "$(cat "$VERIFY/v/f.txt")" = "old" ]
    run grep -r SECRET-OUTSIDE "$VERIFY/v" "$REMOTE_DIR"
    [ "$status" -ne 0 ]
    rm -rf "$VERIFY"
}

@test "scan race: a file replaced by a FIFO after listing does not hang the scan" {
    echo old > f.txt
    echo stable-old > stable.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo stable-new > stable.txt
    SWAP=swap_file_for_fifo
    push_with_swap

    # 124 would mean `timeout` killed a scan that blocked opening the FIFO.
    [ "$status" -eq 0 ]
    [[ "$output" == *"actively changing path"* ]]

    verify_remote
    [ "$(cat "$VERIFY/v/f.txt")" = "old" ]
    rm -rf "$VERIFY"
}

@test "scan race: an unmodified tree still pushes and is a no-op the second time" {
    mkdir -p d/sub
    echo one > d/sub/a.txt
    echo two > top.txt
    ln -s top.txt link
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    [[ "$output" == *"Nothing to push"* ]]
}

@test "filter discovery does not follow directory symlinks (self-referencing links do not blow up)" {
    mkdir a
    echo x > a/f.txt
    ln -s . a/l1
    ln -s . a/l2
    ln -s . a/l3

    run timeout 30 "$OMEMFS" ls
    [ "$status" -eq 0 ]

    run timeout 30 "$OMEMFS" push
    [ "$status" -eq 0 ]
}
