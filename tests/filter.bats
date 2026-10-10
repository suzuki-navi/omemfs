#!/usr/bin/env bats
# Tests for .omemfs-filter (ignore and aggregate behaviour)

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# ---------------------------------------------------------------------------
# Default template creation on clone
# ---------------------------------------------------------------------------

@test "clone: creates .omemfs-filter when it does not exist" {
    [ -f ".omemfs-filter" ]
}

@test "clone: .omemfs-filter contains [ignore] section" {
    grep -q '\[ignore\]' .omemfs-filter
}

@test "clone: .omemfs-filter contains [aggregate] section" {
    grep -q '\[aggregate\]' .omemfs-filter
}

@test "clone: .omemfs-filter contains [line_merge] section" {
    grep -q '\[line_merge\]' .omemfs-filter
    grep -q '\.zsh_history' .omemfs-filter
}

@test "clone: does not overwrite existing .omemfs-filter" {
    echo "custom content" > .omemfs-filter
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" newdir
    [ "$status" -eq 0 ]
    # The new clone should also get the default template (not "custom content")
    [ -f newdir/.omemfs-filter ]
    grep -q '\[ignore\]' newdir/.omemfs-filter
}

# ---------------------------------------------------------------------------
# Ignore: files matching [ignore] patterns are excluded from push
# ---------------------------------------------------------------------------

@test "ignore: ignored file is not pushed to remote" {
    cat > .omemfs-filter <<'EOF'
[ignore]
secret.txt
EOF
    echo "ignored" > secret.txt
    echo "tracked" > tracked.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Clone to fresh dir and verify secret.txt is absent
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ -f verify/tracked.txt ]
    [ ! -f verify/secret.txt ]
}

@test "ignore: ignored directory is not pushed to remote" {
    cat > .omemfs-filter <<'EOF'
[ignore]
build/
EOF
    mkdir build
    echo "artifact" > build/output.bin
    echo "source" > main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ -f verify/main.rs ]
    [ ! -d verify/build ]
}

@test "ignore: anchored pattern excludes only root-level match" {
    cat > .omemfs-filter <<'EOF'
[ignore]
/build
EOF
    mkdir -p build sub/build
    echo "root artifact" > build/out.bin
    echo "sub artifact" > sub/build/out.bin
    echo "source" > main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ -f verify/main.rs ]
    [ ! -d verify/build ]
    # sub/build is NOT anchored-excluded, so it should be present
    [ -f verify/sub/build/out.bin ]
}

@test "ignore: non-anchored pattern excludes nested match" {
    cat > .omemfs-filter <<'EOF'
[ignore]
build
EOF
    mkdir -p sub/build
    echo "nested artifact" > sub/build/out.bin
    echo "source" > main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ -f verify/main.rs ]
    [ ! -d verify/sub/build ]
}

@test "ignore: glob pattern excludes matching files" {
    cat > .omemfs-filter <<'EOF'
[ignore]
**/*.pyc
EOF
    mkdir -p src
    echo "compiled" > src/module.pyc
    echo "source" > src/module.py
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ -f verify/src/module.py ]
    [ ! -f verify/src/module.pyc ]
}

@test "ignore: .omemfs-filter itself is synced (tracked)" {
    cat > .omemfs-filter <<'EOF'
[ignore]
build/
EOF
    echo "tracked" > tracked.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    # .omemfs-filter is tracked and should be present in the clone
    [ -f verify/.omemfs-filter ]
}

@test "ignore: negation pattern re-includes file (file-level ignore)" {
    cat > .omemfs-filter <<'EOF'
[ignore]
*.log
!important.log
EOF
    echo "noise" > debug.log
    echo "important" > important.log
    echo "source" > main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ -f verify/main.rs ]
    [ ! -f verify/debug.log ]
    [ -f verify/important.log ]
}

# ---------------------------------------------------------------------------
# Ignore: subdirectory .omemfs-filter
# ---------------------------------------------------------------------------

@test "ignore: subdirectory filter applies only within that directory" {
    mkdir -p sub
    cat > sub/.omemfs-filter <<'EOF'
[ignore]
/dist
EOF
    mkdir -p sub/dist dist
    echo "sub artifact" > sub/dist/out.bin
    echo "root artifact" > dist/out.bin
    echo "source" > main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ -f verify/main.rs ]
    [ ! -d verify/sub/dist ]
    # root dist/ is not excluded by sub/.omemfs-filter
    [ -f verify/dist/out.bin ]
}

# ---------------------------------------------------------------------------
# Regression: `.omemfs-filter` must round-trip through push/pull/clone/ls
# without ever being rejected as a reserved tree-entry name.
#
# `.omemfs-filter` is a *tracked* file (see the "clone: creates .omemfs-filter"
# tests above and design/09_reserved_names.md), so `omemfs clone --new`'s
# default filter file at the repository root ends up as an ordinary entry in
# every root tree object. `Tree::deserialise`'s reserved-name check
# (design/09 "Reserved names in remote tree entries") must special-case this
# one name, or every command that reads a tree containing it -- i.e. nearly
# every command on nearly every repository -- fails with
# `Error::InvalidObject: tree entry name must not be in the reserved
# .omemfs- namespace`. This was exactly the regression fixed alongside the
# reserved-name checks added by this branch: the checks initially rejected
# `.omemfs-` names uniformly, with no `.omemfs-filter` exception, which broke
# push/pull/clone/ls across the board (see the revert commit
# "Revert Phase 2 reserved-name rejection: breaks .omemfs-filter, a
# legitimately tracked file"). Kept here as an explicit, minimal,
# `.omemfs-filter`-focused regression test in addition to the (numerous, but
# incidental) existing bats coverage that already exercises the same tree
# round-trip indirectly.
# ---------------------------------------------------------------------------

@test "push+ls+pull+clone: default .omemfs-filter round-trips through every tree-reading command" {
    # setup_repo's `clone --new` already wrote the default .omemfs-filter;
    # push it as part of an ordinary tree so it appears in the root tree
    # object read back by every command below.
    echo "hello world" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # `ls` reads the clone-root tree, which contains the .omemfs-filter entry.
    run "$OMEMFS" ls --full-hash file.txt
    [ "$status" -eq 0 ]
    [[ "$output" != *"invalid object"* ]]
    [[ "$output" == *"file.txt"* ]]

    # `pull` reads the remote-root tree, same entry.
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]

    # `clone --existing` reads the remote-root tree from scratch.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" verify
    [ "$status" -eq 0 ]
    [ -f verify/.omemfs-filter ]
    [ -f verify/file.txt ]
}
