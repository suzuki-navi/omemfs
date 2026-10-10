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
