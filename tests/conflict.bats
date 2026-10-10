#!/usr/bin/env bats
# Integration tests for `omemfs conflict`.

load test_helper/common

setup() {
    setup_repo
    echo "hello" > file.txt
    mkdir -p src
    echo "world" > src/main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
}

teardown() {
    teardown_test_dir
    teardown_local_remote
}

# ---------------------------------------------------------------------------
# Helper: create conflict helper files for a path
# ---------------------------------------------------------------------------

make_conflict() {
    local path="$1"
    echo "base content" > "${path}.omemfs-conflict-base"
    echo "local change" > "${path}.omemfs-conflict-local"
    echo "remote change" > "${path}.omemfs-conflict-remote"
}

# ---------------------------------------------------------------------------
# omemfs conflict list
# ---------------------------------------------------------------------------

@test "conflict list: prints nothing when no conflicts exist" {
    run "$OMEMFS" conflict list
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

@test "conflict list: lists conflicting paths" {
    make_conflict file.txt
    run "$OMEMFS" conflict list
    [ "$status" -eq 0 ]
    [[ "$output" == *"file.txt"* ]]
    [[ "$output" != *"omemfs-conflict"* ]]
}

@test "conflict list: lists multiple conflicting paths" {
    make_conflict file.txt
    make_conflict src/main.rs
    run "$OMEMFS" conflict list
    [ "$status" -eq 0 ]
    [[ "$output" == *"file.txt"* ]]
    [[ "$output" == *"src/main.rs"* ]]
}

@test "conflict list: does not list helper files as separate entries" {
    make_conflict file.txt
    run "$OMEMFS" conflict list
    [ "$status" -eq 0 ]
    [[ "$output" != *".omemfs-conflict-base"* ]]
    [[ "$output" != *".omemfs-conflict-local"* ]]
    [[ "$output" != *".omemfs-conflict-remote"* ]]
}

@test "conflict list: scoped to a path lists only that subtree" {
    make_conflict file.txt
    make_conflict src/main.rs
    run "$OMEMFS" conflict list src/
    [ "$status" -eq 0 ]
    [[ "$output" == *"src/main.rs"* ]]
    [[ "$output" != *"file.txt"* ]]
}

@test "conflict list: rejects an outside scope" {
    local outside
    outside="$(mktemp -d)"
    make_conflict file.txt

    run "$OMEMFS" conflict list "$outside"
    [ "$status" -ne 0 ]

    rm -rf "$outside"
}

# ---------------------------------------------------------------------------
# omemfs conflict clean
# ---------------------------------------------------------------------------

@test "conflict clean: removes all three helper files" {
    make_conflict file.txt
    run "$OMEMFS" conflict clean
    [ "$status" -eq 0 ]
    [ ! -f "file.txt.omemfs-conflict-base" ]
    [ ! -f "file.txt.omemfs-conflict-local" ]
    [ ! -f "file.txt.omemfs-conflict-remote" ]
}

@test "conflict clean: leaves the original file intact" {
    make_conflict file.txt
    run "$OMEMFS" conflict clean
    [ "$status" -eq 0 ]
    [ -f "file.txt" ]
    [[ "$(cat file.txt)" == "hello" ]]
}

@test "conflict clean: dry-run does not delete helper files" {
    make_conflict file.txt
    run "$OMEMFS" conflict clean --dry-run
    [ "$status" -eq 0 ]
    [ -f "file.txt.omemfs-conflict-base" ]
    [ -f "file.txt.omemfs-conflict-local" ]
    [ -f "file.txt.omemfs-conflict-remote" ]
}

@test "conflict clean: scoped to a path cleans only that subtree" {
    make_conflict file.txt
    make_conflict src/main.rs
    run "$OMEMFS" conflict clean src/
    [ "$status" -eq 0 ]
    [ ! -f "src/main.rs.omemfs-conflict-base" ]
    [ ! -f "src/main.rs.omemfs-conflict-local" ]
    [ ! -f "src/main.rs.omemfs-conflict-remote" ]
    # file.txt helper files must remain
    [ -f "file.txt.omemfs-conflict-base" ]
    [ -f "file.txt.omemfs-conflict-local" ]
    [ -f "file.txt.omemfs-conflict-remote" ]
}

@test "conflict clean: succeeds with no-op when no conflict files exist" {
    run "$OMEMFS" conflict clean
    [ "$status" -eq 0 ]
}

@test "conflict clean: scope prefix does not match sibling with shared name prefix" {
    # Regression: scope "foo" must not match "foobar/..." (a sibling directory
    # whose name merely starts with the scope string).
    mkdir -p foo foobar
    make_conflict foo/a.txt
    make_conflict foobar/b.txt
    run "$OMEMFS" conflict clean foo
    [ "$status" -eq 0 ]
    # foo/a.txt helpers cleaned.
    [ ! -f "foo/a.txt.omemfs-conflict-base" ]
    # foobar/b.txt helpers must remain untouched.
    [ -f "foobar/b.txt.omemfs-conflict-base" ]
    [ -f "foobar/b.txt.omemfs-conflict-local" ]
    [ -f "foobar/b.txt.omemfs-conflict-remote" ]
}

@test "conflict accept-remote: scope prefix does not match sibling with shared name prefix" {
    # Regression: accept-remote scoped to "foo" must not touch "foobar/...".
    mkdir -p foo foobar
    make_conflict foo/a.txt
    make_conflict foobar/b.txt
    run "$OMEMFS" conflict accept-remote foo
    [ "$status" -eq 0 ]
    [ ! -f "foo/a.txt.omemfs-conflict-base" ]
    [ -f "foobar/b.txt.omemfs-conflict-base" ]
}

# ---------------------------------------------------------------------------
# omemfs conflict accept-remote
# ---------------------------------------------------------------------------

@test "conflict accept-remote: overwrites file with remote content" {
    make_conflict file.txt
    run "$OMEMFS" conflict accept-remote file.txt
    [ "$status" -eq 0 ]
    [[ "$(cat file.txt)" == "remote change" ]]
}

@test "conflict accept-remote: removes all three helper files" {
    make_conflict file.txt
    run "$OMEMFS" conflict accept-remote file.txt
    [ "$status" -eq 0 ]
    [ ! -f "file.txt.omemfs-conflict-base" ]
    [ ! -f "file.txt.omemfs-conflict-local" ]
    [ ! -f "file.txt.omemfs-conflict-remote" ]
}

@test "conflict accept-remote: dry-run does not modify files" {
    make_conflict file.txt
    run "$OMEMFS" conflict accept-remote --dry-run file.txt
    [ "$status" -eq 0 ]
    [[ "$(cat file.txt)" == "hello" ]]
    [ -f "file.txt.omemfs-conflict-remote" ]
}

@test "conflict accept-remote: missing remote helper deletes the file (local-add vs remote-delete)" {
    # Only base and local helper exist (remote deleted the file)
    echo "base content" > file.txt.omemfs-conflict-base
    echo "local change" > file.txt.omemfs-conflict-local
    run "$OMEMFS" conflict accept-remote file.txt
    [ "$status" -eq 0 ]
    [ ! -f "file.txt" ]
    [ ! -f "file.txt.omemfs-conflict-base" ]
    [ ! -f "file.txt.omemfs-conflict-local" ]
}

@test "conflict accept-remote: errors when no conflict files found" {
    run "$OMEMFS" conflict accept-remote file.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"no conflict"* ]] || [[ "$stderr" == *"no conflict"* ]]
}

# ---------------------------------------------------------------------------
# omemfs conflict accept-local
# ---------------------------------------------------------------------------

@test "conflict accept-local: overwrites file with local content" {
    make_conflict file.txt
    echo "current local" > file.txt
    # Replace local helper with current local state
    echo "current local" > file.txt.omemfs-conflict-local
    run "$OMEMFS" conflict accept-local file.txt
    [ "$status" -eq 0 ]
    [[ "$(cat file.txt)" == "current local" ]]
}

@test "conflict accept-local: removes all three helper files" {
    make_conflict file.txt
    run "$OMEMFS" conflict accept-local file.txt
    [ "$status" -eq 0 ]
    [ ! -f "file.txt.omemfs-conflict-base" ]
    [ ! -f "file.txt.omemfs-conflict-local" ]
    [ ! -f "file.txt.omemfs-conflict-remote" ]
}

@test "conflict accept-local: missing local helper deletes the file (local-delete vs remote-modify)" {
    # Only base and remote helper exist (local deleted the file)
    echo "base content" > file.txt.omemfs-conflict-base
    echo "remote change" > file.txt.omemfs-conflict-remote
    run "$OMEMFS" conflict accept-local file.txt
    [ "$status" -eq 0 ]
    [ ! -f "file.txt" ]
    [ ! -f "file.txt.omemfs-conflict-base" ]
    [ ! -f "file.txt.omemfs-conflict-remote" ]
}

# ---------------------------------------------------------------------------
# omemfs conflict accept-base
# ---------------------------------------------------------------------------

@test "conflict accept-base: overwrites file with base content" {
    make_conflict file.txt
    run "$OMEMFS" conflict accept-base file.txt
    [ "$status" -eq 0 ]
    [[ "$(cat file.txt)" == "base content" ]]
}

@test "conflict accept-base: removes all three helper files" {
    make_conflict file.txt
    run "$OMEMFS" conflict accept-base file.txt
    [ "$status" -eq 0 ]
    [ ! -f "file.txt.omemfs-conflict-base" ]
    [ ! -f "file.txt.omemfs-conflict-local" ]
    [ ! -f "file.txt.omemfs-conflict-remote" ]
}

@test "conflict accept-base: missing base helper deletes the file (both sides added)" {
    # Only local and remote helper exist (file was newly added by both sides)
    echo "local version" > file.txt.omemfs-conflict-local
    echo "remote version" > file.txt.omemfs-conflict-remote
    run "$OMEMFS" conflict accept-base file.txt
    [ "$status" -eq 0 ]
    [ ! -f "file.txt" ]
    [ ! -f "file.txt.omemfs-conflict-local" ]
    [ ! -f "file.txt.omemfs-conflict-remote" ]
}

# ---------------------------------------------------------------------------
# push errors when conflict files remain
# ---------------------------------------------------------------------------

@test "push: errors when unresolved conflict files exist" {
    make_conflict file.txt
    echo "modified locally" > file.txt
    run "$OMEMFS" push
    [ "$status" -ne 0 ]
    [[ "$output" == *"conflict"* ]] || [[ "$stderr" == *"conflict"* ]]
}

# ---------------------------------------------------------------------------
# Executable-bit (mode) preservation on accept
# ---------------------------------------------------------------------------

@test "conflict accept-remote: executable bit of the original file is preserved" {
    chmod +x file.txt
    make_conflict file.txt
    run "$OMEMFS" conflict accept-remote file.txt
    [ "$status" -eq 0 ]
    [ "$(cat file.txt)" = "remote change" ]
    # Helper files are never executable; accepting must not clobber the
    # original file's permissions.
    [ -x file.txt ]
}


# ---------------------------------------------------------------------------
# Scope validation and cwd-relative paths
# ---------------------------------------------------------------------------

@test "conflict clean: rejects an outside scope without cleaning the working tree" {
    local outside
    outside="$(mktemp -d)"
    make_conflict file.txt

    run "$OMEMFS" conflict clean "$outside"
    [ "$status" -ne 0 ]
    [ -f file.txt.omemfs-conflict-base ]
    [ -f file.txt.omemfs-conflict-local ]
    [ -f file.txt.omemfs-conflict-remote ]

    rm -rf "$outside"
}

@test "conflict accept-remote: rejects an outside scope without accepting another conflict" {
    local outside
    outside="$(mktemp -d)"
    make_conflict file.txt

    run "$OMEMFS" conflict accept-remote "$outside"
    [ "$status" -ne 0 ]
    [ "$(cat file.txt)" = "hello" ]
    [ -f file.txt.omemfs-conflict-remote ]

    rm -rf "$outside"
}

@test "conflict accept-remote: resolves a relative scope from the current directory" {
    mkdir -p nested
    echo "nested original" > nested/file.txt
    make_conflict nested/file.txt

    cd nested
    run "$OMEMFS" conflict accept-remote file.txt
    [ "$status" -eq 0 ]
    [ "$(cat file.txt)" = "remote change" ]
    [ ! -f file.txt.omemfs-conflict-remote ]
}

@test "conflict list: with no path argument, defaults to the cwd subtree, not the repo root" {
    make_conflict file.txt
    make_conflict src/main.rs

    cd src
    run "$OMEMFS" conflict list
    [ "$status" -eq 0 ]
    [[ "$output" == *"src/main.rs"* ]]
    [[ "$output" != *"file.txt"* ]]
}

@test "conflict clean: with no path argument, defaults to the cwd subtree, not the repo root" {
    make_conflict file.txt
    make_conflict src/main.rs

    cd src
    run "$OMEMFS" conflict clean
    [ "$status" -eq 0 ]
    [ ! -f "main.rs.omemfs-conflict-base" ]
    # file.txt helper files (outside cwd) must remain untouched.
    [ -f "../file.txt.omemfs-conflict-base" ]
    [ -f "../file.txt.omemfs-conflict-local" ]
    [ -f "../file.txt.omemfs-conflict-remote" ]
}

@test "conflict accept-remote: with no path argument, defaults to the cwd subtree, not the repo root" {
    make_conflict file.txt
    make_conflict src/main.rs

    cd src
    run "$OMEMFS" conflict accept-remote
    [ "$status" -eq 0 ]
    [ "$(cat main.rs)" = "remote change" ]
    [ ! -f "main.rs.omemfs-conflict-remote" ]
    # file.txt (outside cwd) must remain an unresolved conflict.
    [ -f "../file.txt.omemfs-conflict-remote" ]
    [ "$(cat ../file.txt)" = "hello" ]
}

# ---------------------------------------------------------------------------
# Symlink traversal safety
# ---------------------------------------------------------------------------

@test "conflict commands: skip helper files below a directory symlink" {
    local outside
    outside="$(mktemp -d)"
    echo "outside conflict" > "$outside/out.omemfs-conflict-remote"
    ln -s "$outside" escape

    run "$OMEMFS" conflict list
    [ "$status" -eq 0 ]
    [[ "$output" != *"escape/out"* ]]

    run "$OMEMFS" conflict clean
    [ "$status" -eq 0 ]
    [ -f "$outside/out.omemfs-conflict-remote" ]

    rm escape
    rm -rf "$outside"
}

@test "conflict clean: a symlinked ancestor of a file-scoped path is refused, never followed to delete outside content" {
    # Regression test: `run_clean`'s per-helper delete used to be a bare
    # `std::fs::remove_file`, which follows symlinks. When the scope names a
    # FILE below a symlinked directory (e.g. "sub/file.txt" where "sub" is a
    # symlink), `collect_helper_files`'s file-scope branch checks existence
    # with `Path::is_file()` (which also follows symlinks), so it can
    # collect a helper path that resolves outside work_dir even though
    # nothing under work_dir's own "sub" is a real directory. The delete
    # itself must still refuse to follow "sub" (design/03_sync_model.md,
    # "Write safety: the working-tree write boundary").
    local outside
    outside="$(mktemp -d)"
    echo "outside conflict" > "$outside/file.txt.omemfs-conflict-local"
    ln -s "$outside" sub

    run "$OMEMFS" conflict clean sub/file.txt
    [ "$status" -ne 0 ]

    # The symlink itself, and everything behind it, must be untouched.
    [ -L sub ]
    [ -f "$outside/file.txt.omemfs-conflict-local" ]
    [ "$(cat "$outside/file.txt.omemfs-conflict-local")" = "outside conflict" ]

    rm sub
    rm -rf "$outside"
}

@test "conflict clean: a scope that is itself a directory symlink finds nothing to clean" {
    # `resolve_scope_paths` used to reject a symlinked scope explicitly (a
    # lexical, TOCTOU-prone check removed alongside the fsguard rewrite --
    # design/03_sync_model.md, "Write safety: the working-tree write
    # boundary"). "escape" being a symlink rather than a real directory means
    # `collect_helper_files` never recognises it as a directory scope to
    # recurse into (`is_real_directory` is `lstat`-based), so it is treated as
    # a plain (non-matching) file scope instead: the command finds nothing
    # under it and exits successfully having touched nothing outside the
    # working tree either way.
    local outside
    outside="$(mktemp -d)"
    echo "outside conflict" > "$outside/out.omemfs-conflict-remote"
    ln -s "$outside" escape

    run "$OMEMFS" conflict clean escape
    [ "$status" -eq 0 ]
    [ -f "$outside/out.omemfs-conflict-remote" ]

    rm escape
    rm -rf "$outside"
}

# ---------------------------------------------------------------------------
# L6: restore removes the conflict metadata sidecar, not just the helpers
# ---------------------------------------------------------------------------

@test "restore: single-path restore of a genuine pull conflict removes every conflict artifact, including the meta sidecar (L6)" {
    # A real `pull` conflict (unlike `make_conflict`'s synthetic helper
    # files) also writes the `.omemfs-conflict-meta` sidecar -- see
    # `build_conflict_meta` in src/commands/pull.rs.
    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    echo "local change" > file.txt
    (cd "$clone2" && echo "remote change" > file.txt && "$OMEMFS" push)

    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [ -f file.txt.omemfs-conflict-local ]
    [ -f file.txt.omemfs-conflict-remote ]
    [ -f file.txt.omemfs-conflict-meta ]

    # Restoring the single conflicted path -- not a whole-tree restore --
    # must remove every conflict artifact for it, including the sidecar
    # (the L6 bug: this used to leave the sidecar behind).
    run "$OMEMFS" restore file.txt
    [ "$status" -eq 0 ]
    [ ! -f file.txt.omemfs-conflict-base ]
    [ ! -f file.txt.omemfs-conflict-local ]
    [ ! -f file.txt.omemfs-conflict-remote ]
    [ ! -f file.txt.omemfs-conflict-meta ]

    rm -rf "$clone2"
}

@test "restore: a stray conflict metadata sidecar with no helper files is removed silently on a whole-tree restore (L6)" {
    # Only the sidecar is present (no helpers) -- e.g. left behind by an
    # older build's single-path restore, before this fix. It is
    # omemfs-internal residue, not user content, so a whole-tree restore
    # must remove it WITHOUT reporting a `deleted:` line for it, exactly
    # like an orphaned stub file.
    echo '{"base":{"mtime":null,"mode":null}}' > file.txt.omemfs-conflict-meta

    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ ! -f file.txt.omemfs-conflict-meta ]
    [[ "$output" != *"file.txt.omemfs-conflict-meta"* ]]

    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

@test "conflict list: does not recurse through a symlink loop" {
    ln -s . loop

    run "$OMEMFS" conflict list
    [ "$status" -eq 0 ]

    rm loop
}

# ---------------------------------------------------------------------------
# clone_root advancement on resolution (design/03_sync_model.md, "Advancing
# clone_root on resolution"; design/04_cli_spec.md accept-*'s step 4).
#
# A genuine pull conflict (not `make_conflict`'s synthetic helpers) leaves
# clone_root pinned at the pre-conflict base. Resolving with accept-local,
# accept-remote, or accept-base must advance clone_root's entry for the
# resolved path to the value observed on the remote when the conflict was
# detected, so a subsequent push is recognized as an intentional
# supersession rather than a lost update.
# ---------------------------------------------------------------------------

@test "conflict accept-local: advances clone_root so the following push succeeds" {
    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    echo "local change" > file.txt
    (cd "$clone2" && echo "remote change" > file.txt && "$OMEMFS" push)

    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [ -f file.txt.omemfs-conflict-local ]

    run "$OMEMFS" conflict accept-local file.txt
    [ "$status" -eq 0 ]
    [[ "$(cat file.txt)" == "local change" ]]

    # Without the clone_root advancement this push is wrongly rejected as a
    # lost update, even though the accepted content is a considered response
    # to the remote state that caused the conflict.
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" clone_chk
    [ "$status" -eq 0 ]
    [[ "$(cat clone_chk/file.txt)" == "local change" ]]

    rm -rf "$clone2"
}

@test "conflict accept-local: a later pull does not re-detect the same conflict" {
    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    echo "local change" > file.txt
    (cd "$clone2" && echo "remote change" > file.txt && "$OMEMFS" push)

    run "$OMEMFS" pull
    [ "$status" -ne 0 ]

    run "$OMEMFS" conflict accept-local file.txt
    [ "$status" -eq 0 ]

    # A pull before pushing must not re-report file.txt as conflicting: the
    # advancement records that this remote state was already acknowledged.
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ ! -f file.txt.omemfs-conflict-local ]
    [[ "$(cat file.txt)" == "local change" ]]

    rm -rf "$clone2"
}

@test "conflict accept-remote: advances clone_root so the following push is a clean no-op" {
    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    echo "local change" > file.txt
    (cd "$clone2" && echo "remote change" > file.txt && "$OMEMFS" push)

    run "$OMEMFS" pull
    [ "$status" -ne 0 ]

    run "$OMEMFS" conflict accept-remote file.txt
    [ "$status" -eq 0 ]
    [[ "$(cat file.txt)" == "remote change" ]]

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ ! -f file.txt.omemfs-conflict-remote ]

    rm -rf "$clone2"
}

# ---------------------------------------------------------------------------
# D10 regression: a remote whole-directory delete conflicting with a local
# edit of one file inside it must not leave an empty directory behind after
# `accept-remote` -- an unpruned empty directory would resurrect the deleted
# directory on the next push (design/03_sync_model.md, "Pruning directories
# emptied by conflict resolution").
# ---------------------------------------------------------------------------

@test "conflict accept-remote: prunes a directory emptied by resolving a remote whole-directory delete vs local edit conflict (D10)" {
    mkdir -p d
    echo "one" > d/f
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    # Remote (via clone2) deletes the whole "d" directory, including its
    # only file.
    (cd "$clone2" && rm -rf d && "$OMEMFS" push)

    # Local edits d/f, unaware that the remote deleted the whole directory.
    echo "local edit" > d/f
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [ -f d/f.omemfs-conflict-local ]
    [ ! -f d/f.omemfs-conflict-remote ]

    run "$OMEMFS" conflict accept-remote d/f
    [ "$status" -eq 0 ]

    # accept-remote must not merely delete d/f -- it must also prune the
    # now-empty "d" directory it leaves behind.
    [ ! -e d ]

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [ ! -e d ]

    # Confirm the remote itself never had "d" resurrected: a fresh clone must
    # not contain it either.
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" clone_verify_d10
    [ "$status" -eq 0 ]
    [ ! -e clone_verify_d10/d ]

    rm -rf "$clone2"
}

@test "conflict accept-base: advances clone_root so the following push succeeds" {
    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    echo "local change" > file.txt
    (cd "$clone2" && echo "remote change" > file.txt && "$OMEMFS" push)

    run "$OMEMFS" pull
    [ "$status" -ne 0 ]

    run "$OMEMFS" conflict accept-base file.txt
    [ "$status" -eq 0 ]
    [[ "$(cat file.txt)" == "hello" ]]

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" clone_chk
    [ "$status" -eq 0 ]
    [[ "$(cat clone_chk/file.txt)" == "hello" ]]

    rm -rf "$clone2"
}

@test "conflict accept-local: only the resolved path advances, an unrelated dirty path is unaffected" {
    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    echo "local change" > file.txt
    echo "unrelated local change" > src/main.rs
    (cd "$clone2" && echo "remote change" > file.txt && "$OMEMFS" push)

    run "$OMEMFS" pull
    [ "$status" -ne 0 ]

    run "$OMEMFS" conflict accept-local file.txt
    [ "$status" -eq 0 ]

    # src/main.rs was never part of the conflict; it must still push as an
    # ordinary local modification once file.txt's own conflict is resolved.
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" clone_chk
    [ "$status" -eq 0 ]
    [[ "$(cat clone_chk/file.txt)" == "local change" ]]
    [[ "$(cat clone_chk/src/main.rs)" == "unrelated local change" ]]

    rm -rf "$clone2"
}

@test "conflict accept-local: if the remote advances again before push, push is correctly rejected as a new divergence" {
    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    echo "local change" > file.txt
    (cd "$clone2" && echo "remote change" > file.txt && "$OMEMFS" push)

    run "$OMEMFS" pull
    [ "$status" -ne 0 ]

    run "$OMEMFS" conflict accept-local file.txt
    [ "$status" -eq 0 ]

    # Someone else pushes again while this conflict is being resolved -- the
    # remote value the advancement recorded is now stale.
    (cd "$clone2" && "$OMEMFS" pull && echo "remote change v2" > file.txt && "$OMEMFS" push)

    run "$OMEMFS" push
    [ "$status" -ne 0 ]
    [[ "$output" == *"remote has been updated since last sync"* ]]

    rm -rf "$clone2"
}

# ---------------------------------------------------------------------------
# Ordering guarantee: helpers/sidecar survive a clone_root-write failure
# (design/03_sync_model.md, "Advancing clone_root on resolution", "Ordering
# guarantee").
#
# Fault injection: after a genuine pull conflict on a nested path
# (sub/inner/file.txt, so an intermediate tree must be spliced/rebuilt), make
# the `.omemfs/` directory itself unwritable (chmod 555, not the
# `clone_root` file: a chmod 000 on the file would instead fail
# `read_clone_root`'s *read*, before either the old or the new code has done
# anything path-specific, which would not distinguish the two).
# `repo.write_clone_root` (src/repo.rs) writes via a temp-file-then-rename
# inside `.omemfs/`, so an unwritable `.omemfs/` fails exactly that write
# while every prior read (including `read_clone_root` and the sidecar reads)
# still succeeds. `accept-local` must then fail while writing the advanced
# clone_root, and -- because the fix computes and durably writes clone_root
# BEFORE removing any helper file or sidecar -- every helper file and the
# metadata sidecar must still be present afterward, and `conflict list` must
# still report the path.
#
# On the pre-fix old code this reliably fails even for this single path: the
# old per-path loop deleted that path's three helper files and its
# `.omemfs-conflict-meta` sidecar (which alone carries the recorded
# `remote_resolution`) BEFORE `write_clone_root` was ever called -- that call
# happened once, only after the whole loop, outside it. So the moment
# `write_clone_root` fails here, the helpers/sidecar for this exact path are
# already gone with no way to recompute `remote_resolution`, and `conflict
# list` no longer reports the path even though `clone_root` was never
# advanced -- exactly the unrecoverable state this fix closes (verified by
# hand against the pre-fix binary: it prints "accepted local:
# sub/inner/file.txt", deletes every helper/sidecar, then fails on the
# clone_root write; afterward `conflict list` prints nothing and `push`
# fails with "remote has been updated since last sync", with no path listed
# to explain why). Removing the fault and re-running the same
# `accept-local` must then succeed and let the following `push` succeed
# too.
# ---------------------------------------------------------------------------

@test "conflict accept-local: a clone_root write failure leaves helpers/sidecar intact and is recoverable" {
    mkdir -p sub/inner
    echo "one" > sub/inner/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    echo "local change" > sub/inner/file.txt
    (cd "$clone2" && echo "remote change" > sub/inner/file.txt && "$OMEMFS" push)

    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [ -f sub/inner/file.txt.omemfs-conflict-local ]
    [ -f sub/inner/file.txt.omemfs-conflict-remote ]
    [ -f sub/inner/file.txt.omemfs-conflict-meta ]

    chmod 555 .omemfs

    run "$OMEMFS" conflict accept-local sub/inner/file.txt
    chmod 755 .omemfs  # restore before any cleanup/rm -rf
    [ "$status" -ne 0 ]

    # Nothing was removed: the helpers, the sidecar, and the original
    # (still local-content) file are all exactly as before the failed
    # attempt.
    [ -f sub/inner/file.txt.omemfs-conflict-base ]
    [ -f sub/inner/file.txt.omemfs-conflict-local ]
    [ -f sub/inner/file.txt.omemfs-conflict-remote ]
    [ -f sub/inner/file.txt.omemfs-conflict-meta ]

    run "$OMEMFS" conflict list
    [ "$status" -eq 0 ]
    [[ "$output" == *"sub/inner/file.txt"* ]]

    # Recovery: with the fault removed, a plain re-run of the same
    # `accept-local` succeeds (re-splicing the same recorded
    # remote_resolution is idempotent), and the following push succeeds.
    run "$OMEMFS" conflict accept-local sub/inner/file.txt
    [ "$status" -eq 0 ]
    [[ "$(cat sub/inner/file.txt)" == "local change" ]]
    [ ! -f sub/inner/file.txt.omemfs-conflict-local ]
    [ ! -f sub/inner/file.txt.omemfs-conflict-meta ]

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" clone_chk
    [ "$status" -eq 0 ]
    [[ "$(cat clone_chk/sub/inner/file.txt)" == "local change" ]]

    rm -rf "$clone2"
}

# ---------------------------------------------------------------------------
# Lazy tree reads: a clone_root tree object missing from the local cache
# after a lazy clone is fetched transparently through the same LazyTreeStore
# `pull`/`ls` already use (design/03_sync_model.md, "Lazy tree reads";
# "Advancing clone_root on resolution", "Lazy tree reads" paragraph).
#
# Fault injection: delete the LOCAL cache copy of sub/inner/'s clone-root
# tree object (it remains on the remote) before resolving a conflict on
# sub/inner/file.txt. splice_entry/remove_entry must rebuild the
# intermediate sub/inner/ tree, which requires reading that tree object --
# on the pre-fix code (routed through `repo.local_store()` alone, with no
# remote fallback) this fails with ObjectNotFound; after the fix (routed
# through LazyTreeStore) the miss is fetched from the remote on demand and
# accept-local succeeds, healing the local cache as a side effect.
# ---------------------------------------------------------------------------

@test "conflict accept-local: a clone_root tree object missing locally is fetched transparently from the remote" {
    mkdir -p sub/inner
    echo "one" > sub/inner/file.txt
    echo "untouched" > top.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Locate sub/inner's clone-root tree hash.
    run "$OMEMFS" ls -r --full-hash --clone
    [ "$status" -eq 0 ]
    inner_line=$(echo "$output" | grep 'sub/inner/$')
    [[ -n "$inner_line" ]]
    inner_hash=$(echo "$inner_line" | grep -oE '[0-9a-f]{64}')
    [[ -n "$inner_hash" ]]

    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    echo "local change" > sub/inner/file.txt
    (cd "$clone2" && echo "remote change" > sub/inner/file.txt && "$OMEMFS" push)

    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [ -f sub/inner/file.txt.omemfs-conflict-local ]

    # Delete sub/inner's clone-root tree object from the LOCAL cache only
    # (adaptive-depth shard layout, src/store/objects_dir.rs -- see the
    # matching comment in tests/ls.bats's local-diff self-healing fixtures
    # for why a suffix match is used). It remains on the remote.
    obj_path=$(find .omemfs/objects -type f -name "*${inner_hash: -40}" | head -1)
    [ -n "$obj_path" ]
    [ -f "$obj_path" ]
    rm -f "$obj_path"

    # accept-local must still succeed: splicing sub/inner/file.txt's new
    # clone_root entry requires rebuilding the sub/inner/ intermediate tree,
    # which requires reading the now-locally-missing object above -- routed
    # through LazyTreeStore, this is healed from the remote rather than
    # failing.
    run "$OMEMFS" conflict accept-local sub/inner/file.txt
    [ "$status" -eq 0 ]
    [[ "$(cat sub/inner/file.txt)" == "local change" ]]
    [ ! -f sub/inner/file.txt.omemfs-conflict-local ]

    # Healed as a side effect: the object is cached locally again.
    [ -f "$obj_path" ]

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" clone_chk
    [ "$status" -eq 0 ]
    [[ "$(cat clone_chk/sub/inner/file.txt)" == "local change" ]]
    [[ "$(cat clone_chk/top.txt)" == "untouched" ]]

    rm -rf "$clone2"
}

# ---------------------------------------------------------------------------
# D9 regression: a conflict whose remote side turned the path into a symlink
# (while the local side edited its content independently) must converge
# under accept-remote -- a symlink created, mtime restored, clone_root
# advanced -- rather than the pre-fix behaviour (file deleted, clone_root
# left unadvanced, so every following push/pull kept failing).
# See design/03_sync_model.md, "Remote side is a symlink".
# ---------------------------------------------------------------------------

@test "conflict accept-remote: resolves a conflict where the remote side became a symlink (D9)" {
    echo "content" > f.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    # Remote (via clone2) replaces f.txt with a symlink.
    (cd "$clone2" && rm f.txt && ln -s target.txt f.txt && "$OMEMFS" push)

    # Local independently edits f.txt's content, unaware of the remote change.
    echo "local edit" > f.txt
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    [ -f f.txt.omemfs-conflict-local ]
    [ ! -f f.txt.omemfs-conflict-remote ]
    # The sidecar must record the remote's symlink shape, not fall back to
    # "unrecorded" (which would leave clone_root unadvanced). serde_json's
    # pretty-printer inserts a space after each key's colon.
    grep -q '"kind": *"symlink"' f.txt.omemfs-conflict-meta
    grep -q '"target": *"target.txt"' f.txt.omemfs-conflict-meta

    run "$OMEMFS" conflict accept-remote f.txt
    [ "$status" -eq 0 ]

    # accept-remote must create the symlink itself, not merely delete the file.
    [ -L f.txt ]
    [ "$(readlink f.txt)" = "target.txt" ]
    [ ! -f f.txt.omemfs-conflict-local ]
    [ ! -f f.txt.omemfs-conflict-meta ]

    # clone_root must be advanced: the following push is a clean no-op, not a
    # CAS failure.
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    [[ "$output" == *"Nothing to push"* ]]

    # A later pull must not re-detect the same conflict.
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    [[ "$output" == *"Already up to date"* ]]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" clone_verify_d9
    [ "$status" -eq 0 ]
    [ -L clone_verify_d9/f.txt ]
    [ "$(readlink clone_verify_d9/f.txt)" = "target.txt" ]

    rm -rf "$clone2"
}

@test "conflict accept-local: a conflict where the remote side became a symlink still advances clone_root to the symlink (D9)" {
    echo "content" > f.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]

    (cd "$clone2" && rm f.txt && ln -s target.txt f.txt && "$OMEMFS" push)

    echo "local edit" > f.txt
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]

    run "$OMEMFS" conflict accept-local f.txt
    [ "$status" -eq 0 ]
    # accept-local keeps the local content, not the symlink.
    [[ "$(cat f.txt)" == "local edit" ]]

    # clone_root was still advanced to the remote's symlink value, so push
    # succeeds as a deliberate supersession (not a lost-update CAS failure).
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" clone_verify_d9_local
    [ "$status" -eq 0 ]
    [[ "$(cat clone_verify_d9_local/f.txt)" == "local edit" ]]

    rm -rf "$clone2"
}

# ---------------------------------------------------------------------------
# The sidecar records which sides exist (design/03_sync_model.md, "The sidecar
# records which sides exist"). accept-* decides from the recorded `sides`, not
# from helper-file presence alone, and refuses to guess when the helpers on
# disk contradict it.
# ---------------------------------------------------------------------------

# Create a genuine edit-vs-edit conflict on file.txt in the current clone.
make_genuine_conflict() {
    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]
    echo "local change" > file.txt
    (cd "$clone2" && echo "remote change" > file.txt && "$OMEMFS" push)
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    rm -rf "$clone2"
}

# Create a genuine local-edit vs remote-delete conflict on file.txt.
make_remote_delete_conflict() {
    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]
    echo "local change" > file.txt
    (cd "$clone2" && rm file.txt && "$OMEMFS" push)
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    rm -rf "$clone2"
}

@test "conflict: pull records which sides exist in the metadata sidecar" {
    make_genuine_conflict
    [ -f file.txt.omemfs-conflict-meta ]
    grep -q '"sides"' file.txt.omemfs-conflict-meta
    ! grep -q '"remote": false' file.txt.omemfs-conflict-meta
}

@test "conflict accept-remote: aborts without changes when a recorded remote helper is missing" {
    make_genuine_conflict
    rm file.txt.omemfs-conflict-remote

    run "$OMEMFS" conflict accept-remote file.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"inconsistent"* ]]
    [[ "$output" == *"file.txt"* ]]

    # Nothing was touched: the working file (which accept-remote would have
    # deleted as a "missing side") and every remaining artifact are intact.
    [ "$(cat file.txt)" = "local change" ]
    [ -f file.txt.omemfs-conflict-local ]
    [ -f file.txt.omemfs-conflict-base ]
    [ -f file.txt.omemfs-conflict-meta ]

    # Nor was clone_root advanced: pulling again still reports the conflict.
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
}

@test "conflict accept-local: aborts without changes when a recorded local helper is missing" {
    make_genuine_conflict
    rm file.txt.omemfs-conflict-local

    run "$OMEMFS" conflict accept-local file.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"inconsistent"* ]]
    [ "$(cat file.txt)" = "local change" ]
    [ -f file.txt.omemfs-conflict-meta ]
}

@test "conflict accept-remote: aborts when a helper exists for a side the sidecar records as absent" {
    make_remote_delete_conflict
    [ ! -f file.txt.omemfs-conflict-remote ]

    # A stray remote helper appears (e.g. copied in by hand). It must not be
    # applied, and must not be silently ignored either.
    echo "stray content" > file.txt.omemfs-conflict-remote

    run "$OMEMFS" conflict accept-remote file.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"inconsistent"* ]]
    [ "$(cat file.txt)" = "local change" ]
    [ -f file.txt.omemfs-conflict-meta ]
}

@test "conflict accept-remote: a consistent remote-delete conflict still deletes the file" {
    make_remote_delete_conflict
    grep -q '"remote": false' file.txt.omemfs-conflict-meta

    run "$OMEMFS" conflict accept-remote file.txt
    [ "$status" -eq 0 ]
    [ ! -f file.txt ]
    [ ! -f file.txt.omemfs-conflict-meta ]
}

@test "conflict accept-remote --dry-run: reports the same inconsistency error and changes nothing" {
    make_genuine_conflict
    rm file.txt.omemfs-conflict-remote

    run "$OMEMFS" conflict accept-remote --dry-run file.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"inconsistent"* ]]
    [ "$(cat file.txt)" = "local change" ]
    [ -f file.txt.omemfs-conflict-meta ]
}

@test "conflict accept-remote: one inconsistent path aborts the whole multi-path resolution before any change" {
    echo "other" > other.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local clone2
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]
    echo "local change" > file.txt
    echo "local other" > other.txt
    (cd "$clone2" && echo "remote change" > file.txt && echo "remote other" > other.txt && "$OMEMFS" push)
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    rm -rf "$clone2"

    rm other.txt.omemfs-conflict-remote

    run "$OMEMFS" conflict accept-remote
    [ "$status" -ne 0 ]
    [[ "$output" == *"other.txt"* ]]

    # file.txt was consistent, but nothing is applied when any path is not.
    [ "$(cat file.txt)" = "local change" ]
    [ -f file.txt.omemfs-conflict-remote ]
    [ -f file.txt.omemfs-conflict-meta ]
}

@test "conflict accept-remote: a sidecar without recorded sides falls back to helper-file presence" {
    make_conflict file.txt
    echo '{"remote_resolution":{"kind":"unrecorded"}}' > file.txt.omemfs-conflict-meta

    run "$OMEMFS" conflict accept-remote file.txt
    [ "$status" -eq 0 ]
    [ "$(cat file.txt)" = "remote change" ]
}
