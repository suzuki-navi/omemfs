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

@test "ls: paths are displayed relative to the cwd" {
    seed_tree
    cd sub
    run "$OMEMFS" ls a.txt
    [ "$status" -eq 0 ]
    # cwd-relative display path: not root-anchored.
    [[ "$output" == *" a.txt" ]]
    [[ "$output" != *"sub/a.txt"* ]]
}

@test "ls: a displayed path can be passed back to push, restore and expand from the same cwd" {
    seed_tree
    cd sub/deep
    echo "b2" > b.txt
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    shown=$(echo "$output" | awk '{print $NF}' | grep 'b.txt')
    [ "$shown" = "b.txt" ]
    # The displayed name is a valid argument from this same directory.
    run "$OMEMFS" restore "$shown"
    [ "$status" -eq 0 ]
    [ "$(cat b.txt)" = "b" ]
}

@test "ls: the listed directory itself is the '.' row when run in a subdirectory" {
    seed_tree
    cd sub
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    first=$(echo "$output" | head -1)
    [[ "$first" == *" ." ]]
    [[ "$output" == *" deep/"* ]]
    [[ "$output" == *" a.txt"* ]]
    [[ "$output" != *"sub/"* ]]
}

@test "ls -r: recursive output from a subdirectory is cwd-relative" {
    seed_tree
    cd sub
    run "$OMEMFS" ls -r
    [ "$status" -eq 0 ]
    [[ "$output" == *" deep/b.txt"* ]]
    [[ "$output" != *"sub/deep/b.txt"* ]]
}

@test "ls: relative path from a subdirectory resolves against cwd" {
    seed_tree
    cd sub
    run "$OMEMFS" ls deep
    [ "$status" -eq 0 ]
    [[ "$output" == *" deep/"* ]]
    [[ "$output" == *"deep/b.txt"* ]]
    [[ "$output" != *"sub/deep"* ]]
}

@test "ls: .. climbs out of the subdirectory" {
    seed_tree
    cd sub/deep
    run "$OMEMFS" ls ../a.txt
    [ "$status" -eq 0 ]
    # `a.txt` is in `sub`, one level above the cwd `sub/deep`.
    [[ "$output" == *" ../a.txt"* ]]
}

@test "ls: path outside the repository is rejected" {
    seed_tree
    cd sub
    run "$OMEMFS" ls ../../etc
    [ "$status" -ne 0 ]
    [[ "$output" == *"outside the repository"* ]]
}

@test "absolute path through a symlinked repository root resolves like the real one" {
    seed_tree
    local real link
    real="$(pwd -P)"
    link="$(mktemp -d)/link"
    ln -s "$real" "$link"
    cd "$link"
    # cwd reached via the symlink; the absolute argument spelled both ways.
    run "$OMEMFS" ls "$link/top.txt"
    [ "$status" -eq 0 ]
    [[ "$output" == *"top.txt"* ]]
    run "$OMEMFS" ls "$real/top.txt"
    [ "$status" -eq 0 ]
    [[ "$output" == *"top.txt"* ]]
    # And a path really outside the repository is still rejected.
    run "$OMEMFS" ls /etc/hostname
    [ "$status" -ne 0 ]
    [[ "$output" == *"outside the repository"* ]]
    rm -rf "$(dirname "$link")"
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
    # Only the in-scope change is listed (cwd-relative); the root-only change
    # is not.
    [[ "$output" == *" a.txt"* ]]
    [[ "$output" != *"sub/a.txt"* ]]
    [[ "$output" != *"top.txt"* ]]
}

@test "ls --dirty: relative path from a subdirectory scopes to that path" {
    seed_tree
    echo "a2" > sub/a.txt           # change outside the named scope
    echo "b2" > sub/deep/b.txt      # change inside the named scope
    cd sub
    run "$OMEMFS" ls --dirty deep
    [ "$status" -eq 0 ]
    [[ "$output" == *" deep/b.txt"* ]]
    [[ "$output" != *"sub/"* ]]
    [[ "$output" != *" a.txt"* ]]
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

@test "restore: no-arg from a subdirectory restores only that subtree" {
    seed_tree
    echo "changed" > top.txt
    cd sub
    echo "changed" > a.txt
    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    # The in-subdir change is reverted; the out-of-subdir change is left alone.
    [ "$(cat a.txt)" = "a" ]
    [ "$(cat ../top.txt)" = "changed" ]
}

@test "restore: no-arg at the repository root still restores the whole tree" {
    seed_tree
    echo "changed" > top.txt
    echo "changed" > sub/a.txt
    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ "$(cat top.txt)" = "top" ]
    [ "$(cat sub/a.txt)" = "a" ]
}

@test "restore: '..' from a subdirectory restores the whole tree explicitly" {
    seed_tree
    echo "changed" > top.txt
    cd sub
    echo "changed" > a.txt
    run "$OMEMFS" restore ..
    [ "$status" -eq 0 ]
    [ "$(cat a.txt)" = "a" ]
    [ "$(cat ../top.txt)" = "top" ]
}

@test "expand: no-arg from a subdirectory expands only stubs at or below the cwd" {
    seed_tree
    echo "inside" > sub/stubme.txt
    echo "outside" > outer.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" stub sub/stubme.txt outer.txt
    [ "$status" -eq 0 ]
    [ -f sub/stubme.txt.omemfs-stub ]
    [ -f outer.txt.omemfs-stub ]

    cd sub
    run "$OMEMFS" expand --stub-threshold 0
    [ "$status" -eq 0 ]
    # The stub under the cwd is expanded; the one outside it is untouched.
    [ "$(cat stubme.txt)" = "inside" ]
    [ ! -f stubme.txt.omemfs-stub ]
    [ -f ../outer.txt.omemfs-stub ]
    [ ! -f ../outer.txt ]
}

@test "expand: no-arg at the repository root still expands every stub" {
    seed_tree
    echo "inside" > sub/stubme.txt
    echo "outside" > outer.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" stub sub/stubme.txt outer.txt
    [ "$status" -eq 0 ]
    run "$OMEMFS" expand --stub-threshold 0
    [ "$status" -eq 0 ]
    [ "$(cat sub/stubme.txt)" = "inside" ]
    [ "$(cat outer.txt)" = "outside" ]
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
