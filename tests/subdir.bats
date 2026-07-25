#!/usr/bin/env bats
# Tests for running omemfs from a subdirectory of the working tree.
# The repository is discovered by walking up from the cwd, and relative
# <path> arguments resolve against the cwd (see design/04 "Repository discovery").

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# Seed a small tree: top.txt, sub/a.txt, sub/deep/b.txt — and push it.
seed_tree() {
    echo "top" > top.txt
    mkdir -p sub/deep
    echo "a" > sub/a.txt
    echo "b" > sub/deep/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
}

@test "discover: ls from a subdirectory finds the repo root" {
    seed_tree
    cd sub
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
}

@test "discover: command outside any repo errors clearly" {
    local outside
    outside="$(mktemp -d)"
    cd "$outside"
    run "$OMEMFS" ls
    [ "$status" -ne 0 ]
    [[ "$output" == *"not a omemfs repository"* ]]
    [[ "$output" == *"or any parent"* ]]
    rm -rf "$outside"
}

@test "ls: no-arg from a subdirectory scopes to that subdirectory" {
    seed_tree
    cd sub
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    # The subtree's own file is listed; the root-only file is not.
    [[ "$output" == *"a.txt"* ]]
    [[ "$output" != *"top.txt"* ]]
}

@test "ls: paths are displayed relative to the repository root" {
    seed_tree
    cd sub
    run "$OMEMFS" ls a.txt
    [ "$status" -eq 0 ]
    # Root-anchored display path, not cwd-relative.
    [[ "$output" == *"sub/a.txt"* ]]
}

@test "ls: relative path from a subdirectory resolves against cwd" {
    seed_tree
    cd sub
    run "$OMEMFS" ls deep
    [ "$status" -eq 0 ]
    [[ "$output" == *"sub/deep"* ]]
    [[ "$output" == *"b.txt"* ]]
}

@test "ls: .. climbs out of the subdirectory" {
    seed_tree
    cd sub/deep
    run "$OMEMFS" ls ../a.txt
    [ "$status" -eq 0 ]
    [[ "$output" == *"sub/a.txt"* ]]
}

@test "ls: path outside the repository is rejected" {
    seed_tree
    cd sub
    run "$OMEMFS" ls ../../etc
    [ "$status" -ne 0 ]
    [[ "$output" == *"outside the repository"* ]]
}

@test "push: no-arg from a subdirectory scopes the push to that subtree" {
    seed_tree
    cd sub
    echo "a2" > a.txt          # modify in-scope file
    echo "top2" > ../top.txt   # modify out-of-scope file
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # Out-of-scope change still shows as dirty after the scoped push.
    cd ..
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [[ "$output" == *"top.txt"* ]]
    [[ "$output" != *"sub/a.txt"* ]]
}

@test "ls --dirty: no-arg from a subdirectory scopes to that subdirectory" {
    seed_tree
    echo "a2" > sub/a.txt          # in-scope change
    echo "top2" > top.txt          # out-of-scope change
    cd sub
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    # Only the in-scope change is listed; the root-only change is not.
    [[ "$output" == *"sub/a.txt"* ]]
    [[ "$output" != *"top.txt"* ]]
}

@test "ls --dirty: relative path from a subdirectory scopes to that path" {
    seed_tree
    echo "a2" > sub/a.txt           # change outside the named scope
    echo "b2" > sub/deep/b.txt      # change inside the named scope
    cd sub
    run "$OMEMFS" ls --dirty deep
    [ "$status" -eq 0 ]
    [[ "$output" == *"sub/deep/b.txt"* ]]
    [[ "$output" != *"sub/a.txt"* ]]
}

@test "ls --dirty: clean in-scope subtree prints nothing despite out-of-scope change" {
    seed_tree
    echo "top2" > top.txt           # out-of-scope change only
    cd sub
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

@test "push: relative path from a subdirectory pushes the named file" {
    seed_tree
    cd sub
    echo "a2" > a.txt
    run "$OMEMFS" push a.txt
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

@test "stub: relative path from a subdirectory stubs the right file" {
    seed_tree
    cd sub
    run "$OMEMFS" stub a.txt
    [ "$status" -eq 0 ]
    [ -f a.txt.omemfs-stub ]
}

@test "restore: relative path from a subdirectory restores the right file" {
    seed_tree
    cd sub
    echo "changed" > a.txt
    run "$OMEMFS" restore a.txt
    [ "$status" -eq 0 ]
    [ "$(cat a.txt)" = "a" ]
}

@test "restore: no-arg from a subdirectory restores the whole tree" {
    seed_tree
    echo "changed" > top.txt
    cd sub
    echo "changed" > a.txt
    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    # Both the in-subdir and the out-of-subdir change are reverted.
    [ "$(cat a.txt)" = "a" ]
    [ "$(cat ../top.txt)" = "top" ]
}

@test "pull: no-arg from a subdirectory scopes the pull to that subtree" {
    seed_tree
    # Make a second clone, change sub/a.txt there, and push.
    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]
    run bash -c "cd '$clone2' && echo 'a-remote' > sub/a.txt && echo 'top-remote' > top.txt && '$OMEMFS' push"
    [ "$status" -eq 0 ]
    # Pull only sub/ from the original repo.
    cd sub
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ "$(cat a.txt)" = "a-remote" ]
    # top.txt was out of scope and is unchanged.
    [ "$(cat ../top.txt)" = "top" ]
    rm -rf "$clone2"
}
