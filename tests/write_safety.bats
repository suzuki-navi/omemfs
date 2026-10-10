#!/usr/bin/env bats
# Integration tests for the fsguard-based working-tree write boundary
# (design/03_sync_model.md, "Write safety: the working-tree write boundary").
#
# These cover the specific bug the old `fsmeta::reject_symlink_escape` guard
# missed: a working-tree directory replaced by a symlink whose TARGET itself
# contains a fake `.omemfs/` marker. The old guard re-derived its boundary via
# `Repo::discover` (which uses `.is_dir()`, following symlinks), so it mistook
# the symlink target for the repository root and had nothing left to check.
# `tests/symlink.bats` already covers the plain (no fake marker) case, which
# the old guard did catch.

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
# C1 repro: symlink target carries a fake `.omemfs/` marker.
# ---------------------------------------------------------------------------

@test "write_safety: pull refuses to write through a symlink even when its target has a fake .omemfs marker" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir sub
    echo "secret content" > sub/child.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    local outside
    outside="$(mktemp -d)"
    # The attacker directory carries its own fake .omemfs/ marker -- this is
    # exactly the case that bypassed the old guard, which mistook the symlink
    # target itself for the repository root.
    mkdir -p "$outside/.omemfs"

    cd clone2
    ln -s "$outside" sub
    run "$OMEMFS" pull
    cd ..

    [ "$status" -ne 0 ]
    [ ! -e "$outside/child.txt" ]
    [ -L clone2/sub ]
    [ "$(readlink clone2/sub)" = "$outside" ]

    rm -rf "$outside"
}

@test "write_safety: restore refuses to write through a symlink even when its target has a fake .omemfs marker" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    mkdir sub
    echo "secret content" > sub/child.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    local outside
    outside="$(mktemp -d)"
    mkdir -p "$outside/.omemfs"

    cd clone1
    rm -rf sub
    ln -s "$outside" sub
    run "$OMEMFS" restore sub
    cd ..

    [ "$status" -ne 0 ]
    [ ! -e "$outside/child.txt" ]
    [ -L clone1/sub ]
    [ "$(readlink clone1/sub)" = "$outside" ]

    rm -rf "$outside"
}

@test "write_safety: expand refuses to write through a symlink even when its target has a fake .omemfs marker" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src
    [ "$status" -eq 0 ]

    cd src
    mkdir sub
    echo "secret content" > sub/child.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # A small stub threshold turns "sub" into a single unexpanded directory
    # stub on dest: dest/sub exists on disk as a real (empty) directory
    # containing only the `.omemfs-stub` marker.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 1 dest
    [ "$status" -eq 0 ]
    [ -f dest/sub/.omemfs-stub ]
    [ ! -e dest/sub/child.txt ]

    local outside
    outside="$(mktemp -d)"
    mkdir -p "$outside/.omemfs"
    # Simulate the directory having been swapped for a symlink after clone:
    # carry the stub marker over to $outside so expand's stub-record read
    # (unguarded, by design -- only the write side is in scope here) still
    # succeeds and proceeds to try to materialise "child.txt" there.
    cp dest/sub/.omemfs-stub "$outside/.omemfs-stub"
    rm -rf dest/sub
    ln -s "$outside" dest/sub

    cd dest
    run "$OMEMFS" expand sub
    cd ..

    [ "$status" -ne 0 ]
    [ ! -e "$outside/child.txt" ]
    [ -L dest/sub ]
    [ "$(readlink dest/sub)" = "$outside" ]

    rm -rf "$outside"
}

# ---------------------------------------------------------------------------
# create_dir_all repro: a refused content write must not have created any
# directory inside the symlink target either.
# ---------------------------------------------------------------------------

@test "write_safety: restore's refused content write creates no directories inside the symlink target" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p a/b/c
    echo "secret" > a/b/c/child.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    local outside
    outside="$(mktemp -d)"

    cd clone1
    rm -rf a
    ln -s "$outside" a
    run "$OMEMFS" restore a
    cd ..

    [ "$status" -ne 0 ]
    [ ! -e "$outside/b" ]
    [ -z "$(ls -A "$outside")" ]
    [ -L clone1/a ]

    rm -rf "$outside"
}

# ---------------------------------------------------------------------------
# pull's line-merge write, and the previously-unguarded conflict writers.
# ---------------------------------------------------------------------------

@test "write_safety: pull's scoped line-merge write is refused through a symlinked directory" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p logs
    printf ': 100:0;cmd_100\n' > logs/.zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    cd ..

    local outside
    outside="$(mktemp -d)"
    mkdir -p "$outside/.omemfs"
    printf ': 100:0;cmd_100\n: 200:0;cmd_local\n' > "$outside/.zsh_history"

    cd clone2
    rm -rf logs
    ln -s "$outside" logs
    cd ..

    cd clone1
    printf ': 300:0;cmd_remote\n' >> logs/.zsh_history
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # A scoped pull -- not a full pull -- so `write_resolved_line_merges` is
    # called with the scoped subtree dir, not the repo root, as `base_dir`.
    # The fsguard boundary passed alongside it must still be the true repo
    # root (see src/commands/pull.rs), not that scoped dir itself, or this
    # symlinked "logs" component would never be checked.
    cd clone2
    run "$OMEMFS" pull logs
    cd ..

    [ "$status" -ne 0 ]
    [ "$(cat "$outside/.zsh_history")" = $': 100:0;cmd_100\n: 200:0;cmd_local' ]
    [ -L clone2/logs ]

    rm -rf "$outside"
}

@test "write_safety: conflict accept-remote refuses to write through a symlinked directory" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir sub
    echo "base" > sub/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    echo "local" > sub/file.txt
    cd ..

    cd clone1
    echo "remote" > sub/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [ -f sub/file.txt.omemfs-conflict-base ]
    [ -f sub/file.txt.omemfs-conflict-remote ]
    cd ..

    local outside
    outside="$(mktemp -d)"
    mkdir -p "$outside/.omemfs"
    # Simulate "sub" having been swapped for a symlink after the conflict was
    # detected: carry the helper files over so accept's (unguarded) discovery
    # of the conflict still succeeds and proceeds to try to write the
    # accepted content there.
    cp clone2/sub/file.txt.omemfs-conflict-base "$outside/file.txt.omemfs-conflict-base"
    cp clone2/sub/file.txt.omemfs-conflict-remote "$outside/file.txt.omemfs-conflict-remote"
    if [ -f clone2/sub/file.txt.omemfs-conflict-meta ]; then
        cp clone2/sub/file.txt.omemfs-conflict-meta "$outside/file.txt.omemfs-conflict-meta"
    fi

    cd clone2
    rm -rf sub
    ln -s "$outside" sub
    run "$OMEMFS" conflict accept-remote sub/file.txt
    cd ..

    [ "$status" -ne 0 ]
    [ ! -e "$outside/file.txt" ]
    [ -L clone2/sub ]

    rm -rf "$outside"
}

@test "write_safety: pull's .omemfs-conflict-meta sidecar write is refused through a symlinked directory" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir sub
    echo "base" > sub/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    cd ..

    local outside
    outside="$(mktemp -d)"
    mkdir -p "$outside/.omemfs"
    echo "local" > "$outside/file.txt"

    cd clone2
    rm -rf sub
    ln -s "$outside" sub
    cd ..

    cd clone1
    echo "remote" > sub/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    cd ..

    [ "$status" -ne 0 ]
    [ ! -e "$outside/file.txt.omemfs-conflict-meta" ]
    [ ! -e "$outside/file.txt.omemfs-conflict-base" ]
    [ ! -e "$outside/file.txt.omemfs-conflict-remote" ]
    [ "$(cat "$outside/file.txt")" = "local" ]
    [ -L clone2/sub ]

    rm -rf "$outside"
}

# ---------------------------------------------------------------------------
# N1: a delete driven by a lexically-planned path must not escape either.
# ---------------------------------------------------------------------------

@test "write_safety: restore's delete-absent action refuses to unlink through a symlinked directory (N1)" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]

    cd clone1
    echo "root" > root.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    local outside
    outside="$(mktemp -d)"
    mkdir -p "$outside/.omemfs"
    echo "victim content" > "$outside/x"

    cd clone1
    # "sub" has no entry at all in clone_root; "sub/x" is therefore planned
    # as a delete (Action::DeleteAbsent), computed via a plain `abs.is_file()`
    # probe that follows this symlink straight to $outside/x.
    ln -s "$outside" sub
    run "$OMEMFS" restore sub/x
    cd ..

    [ "$status" -ne 0 ]
    [ -f "$outside/x" ]
    [ "$(cat "$outside/x")" = "victim content" ]
    [ -L clone1/sub ]

    rm -rf "$outside"
}

# ---------------------------------------------------------------------------
# Regression coverage: ordinary writes through real nested directories, and
# a symlinked `work_dir` root itself, must keep working exactly as before.
# ---------------------------------------------------------------------------

@test "write_safety: pull, restore, and expand still work through ordinary real nested directories" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone1
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone2
    [ "$status" -eq 0 ]

    cd clone1
    mkdir -p a/b/c
    echo "content" > a/b/c/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone2
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ "$(cat a/b/c/file.txt)" = "content" ]

    rm a/b/c/file.txt
    run "$OMEMFS" restore a/b/c/file.txt
    [ "$status" -eq 0 ]
    [ "$(cat a/b/c/file.txt)" = "content" ]
    cd ..

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" --stub-threshold 1 clone3
    [ "$status" -eq 0 ]
    cd clone3
    run "$OMEMFS" expand
    [ "$status" -eq 0 ]
    [ "$(cat a/b/c/file.txt)" = "content" ]
    cd ..
}

@test "write_safety: clone into a symlinked directory still works (work_dir itself may be a symlink)" {
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" src
    [ "$status" -eq 0 ]

    cd src
    echo "hello" > file.txt
    mkdir -p sub
    echo "nested" > sub/nested.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    mkdir real_dest
    ln -s "$(pwd)/real_dest" dest_link

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" dest_link
    [ "$status" -eq 0 ]
    [ -f dest_link/file.txt ]
    [ -f dest_link/sub/nested.txt ]
    [ -f real_dest/file.txt ]
    [ -f real_dest/sub/nested.txt ]
}
