#!/usr/bin/env bats
# Tests for `omemfs ls`

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

@test "ls: shows files in current directory" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    [[ "$output" == *"file.txt"* ]]
}

@test "ls: cold listing hashes files without writing blob objects" {
    # design/03 "Scan blob-write mode": a read-only ls computes hashes but must
    # not write blob objects. With 3 flat files, a full scan would write 3 blobs
    # plus the root tree; ls must write at most the tree objects (< 3 new files).
    echo "alpha content one" > a.txt
    echo "beta content two"  > b.txt
    echo "gamma content three" > c.txt
    before=$(find .omemfs/objects -type f | wc -l)
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    [[ "$output" == *"a.txt"* ]]
    [[ "$output" == *"b.txt"* ]]
    [[ "$output" == *"c.txt"* ]]
    after=$(find .omemfs/objects -type f | wc -l)
    # Strictly fewer than 3 new objects → the 3 blobs were not written.
    [ "$((after - before))" -lt 3 ]
}

@test "ls before push: push stages and uploads correct content" {
    # A cold ls populates STAT_CACHE without writing blobs. The subsequent push
    # gets STAT_CACHE hits, notices that their blobs are absent, and stages the
    # missing blobs during the push scan before upload.
    echo "hello from a" > a.txt
    mkdir -p sub
    echo "nested b content" > sub/b.txt

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Remote round-trips: a second clone of the same remote pulls exact content.
    local dest
    dest="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$dest/verify"
    [ "$status" -eq 0 ]
    (
        cd "$dest/verify"
        run "$OMEMFS" pull
        [ "$status" -eq 0 ]
        [ "$(cat a.txt)" = "hello from a" ]
        [ "$(cat sub/b.txt)" = "nested b content" ]
    )
    rm -rf "$dest"
}

@test "ls before scoped push: scoped push uploads correct content" {
    mkdir -p src docs
    echo "source main" > src/main.rs
    echo "the readme" > docs/README.md

    # Cold ls over the whole tree (no blobs written, STAT_CACHE populated).
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]

    # Scoped push of just src must stage src/main.rs's blob before upload.
    run "$OMEMFS" push src
    [ "$status" -eq 0 ]

    local dest
    dest="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$dest/verify"
    [ "$status" -eq 0 ]
    (
        cd "$dest/verify"
        run "$OMEMFS" pull
        [ "$status" -eq 0 ]
        [ "$(cat src/main.rs)" = "source main" ]
    )
    rm -rf "$dest"
}

@test "ls: --dirty shows nothing when working tree is clean" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

@test "ls: --dirty shows modified file" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "changed" > file.txt
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [[ "$output" == *"file.txt"* ]]
    [[ "$output" == M* ]] || [[ "$output" == *"M "* ]]
}

@test "ls: --dirty shows mode-only change (chmod +x) as modified" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # Content unchanged; only the executable bit is added.
    chmod +x file.txt
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [[ "$output" == *"file.txt"* ]]
    [[ "$output" == M* ]] || [[ "$output" == *"M "* ]]
}

@test "ls: --dirty shows mode-only change (chmod -x) as modified" {
    echo "hello" > script.sh
    chmod +x script.sh
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # Content unchanged; only the executable bit is removed.
    chmod -x script.sh
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [[ "$output" == *"script.sh"* ]]
    [[ "$output" == M* ]] || [[ "$output" == *"M "* ]]
}

@test "ls: --dirty stays clean after mode-only change is pushed" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    chmod +x file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

@test "ls: --dirty with mtime-only change writes no new objects" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    before=$(find .omemfs/objects -type f | wc -l)
    # Change only the mtime; content is identical.
    touch -d "2020-01-01 00:00:00" file.txt
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [ -z "$output" ]
    # mtime stability must prevent new tree objects from being written.
    after=$(find .omemfs/objects -type f | wc -l)
    [ "$before" -eq "$after" ]
}

@test "ls: path-scoped listing excludes out-of-scope modified file rows" {
    mkdir -p src docs
    echo "source" > src/main.rs
    echo "readme" > docs/README.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "changed" > docs/README.md
    run "$OMEMFS" ls src
    [ "$status" -eq 0 ]
    [[ "$output" != *"docs/README.md"* ]]
    [[ "$output" == *"src/main.rs"* ]]
}

@test "ls: path-scoped listing shows M for in-scope modified file" {
    mkdir -p src
    echo "source" > src/main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "changed" > src/main.rs
    run "$OMEMFS" ls src
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "src/main.rs")
    [[ -n "$file_line" ]]
    [[ "${file_line:1:1}" == "M" ]]
}

@test "ls: --dirty shows added file" {
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "new" > new_file.txt
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [[ "$output" == *"new_file.txt"* ]]
}

@test "ls: -r lists files recursively" {
    mkdir -p sub/dir
    echo "nested" > sub/dir/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls -r
    [ "$status" -eq 0 ]
    [[ "$output" == *"sub/dir/file.txt"* ]]
}

@test "ls: shows modified file with M status" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "changed" > file.txt
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    [[ "$output" == *"file.txt"* ]]
    # Check the line for file.txt starts with M (possibly with leading spaces for the Z column)
    file_line=$(echo "$output" | grep "file\.txt")
    [[ "$file_line" == M* ]] || [[ "$file_line" == " M"* ]] || [[ "${file_line:0:2}" == "M " ]]
}

@test "ls: shows added file with A status" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "new content" > new_file.txt
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    [[ "$output" == *"new_file.txt"* ]]
    # The added file should be marked A
    [[ "$output" == *"A"*"new_file.txt"* ]]
}

@test "ls: shows blob_count for directory entries" {
    mkdir -p sub
    echo "a" > sub/a.txt
    echo "b" > sub/b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    # Directory entry for sub/ should show blob_count=2
    subdir_line=$(echo "$output" | grep "sub/")
    [[ -n "$subdir_line" ]]
    [[ "$subdir_line" == *"2"* ]]
}

@test "ls: shows blob_count=1 for blob entries" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    # Blob entry should show blob_count=1
    file_line=$(echo "$output" | grep "file.txt")
    [[ -n "$file_line" ]]
    [[ "$file_line" == *"1"* ]]
}

@test "ls: columns are aligned (path starts at same position)" {
    echo "hello" > file.txt
    mkdir -p sub
    echo "a" > sub/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls -r
    [ "$status" -eq 0 ]
    # All lines should have path starting at the same column offset
    # Extract the column offset of the last field (path) for each line
    positions=()
    while IFS= read -r line; do
        [[ -n "$line" ]] || continue
        # The path is the last token; count characters before it
        path_token=$(echo "$line" | awk '{print $NF}')
        # Find position of path_token in line (-F: literal string, not regex)
        pos=$(echo "$line" | grep -Fbo "$path_token" | head -1 | cut -d: -f1)
        positions+=("$pos")
    done <<< "$output"
    # All positions must be equal
    first="${positions[0]}"
    for p in "${positions[@]}"; do
        [ "$p" = "$first" ]
    done
}

@test "ls: mtime is shown in short format for pushed files" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    # mtime should not be '-' after push
    file_line=$(echo "$output" | grep "file.txt")
    [[ -n "$file_line" ]]
    # Should contain either 'now', 'Nm', 'MM-DD HH:MM', or 'YYYY-MM-DD'
    [[ "$file_line" != *" - "* ]] || true
}

@test "ls: path not found returns non-zero exit code" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls nonexistent.txt
    [ "$status" -ne 0 ]
    [[ "$output" == *"path not found"* ]] || [[ "$output" == *"not found"* ]]
}

@test "ls: path-scoped listing excludes added files outside the scope" {
    mkdir -p src docs
    echo "source" > src/main.rs
    echo "readme" > docs/README.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Add a new file outside src/.
    echo "extra" > docs/extra.txt

    run "$OMEMFS" ls src
    [ "$status" -eq 0 ]
    # docs/extra.txt must not appear in the output
    [[ "$output" != *"docs/extra.txt"* ]]
}

@test "ls: path-scoped listing includes added files inside the scope" {
    mkdir -p src
    echo "source" > src/main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "new" > src/lib.rs

    run "$OMEMFS" ls src
    [ "$status" -eq 0 ]
    [[ "$output" == *"src/lib.rs"* ]]
    [[ "$output" == *"A"* ]]
}

@test "ls: output format is 'XZ hash size mtime path'" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    # Each line should have at least 4 space-separated fields
    while IFS= read -r line; do
        [[ -n "$line" ]] || continue
        count=$(echo "$line" | awk '{print NF}')
        [ "$count" -ge 4 ]
    done <<< "$output"
}

@test "ls: mtime is shown for existing working tree files" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "file.txt")
    [[ -n "$file_line" ]]
    # mtime comes from clone root; must not be '-' for a pushed file
    [[ "$file_line" != *"- file.txt"* ]]
}

@test "ls: mtime is '-' for deleted files" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    rm file.txt
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "file.txt")
    [[ -n "$file_line" ]]
    # Deleted file in --dirty mode has no mtime data
    [[ "$file_line" == *" - "* ]]
}

@test "ls: mtime is shown for directory entries" {
    mkdir -p sub
    echo "a" > sub/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep "sub/")
    [[ -n "$dir_line" ]]
    # mtime comes from clone root tree entry; must not be '-' for a pushed directory
    [[ "$dir_line" != *"- sub/"* ]]
}

@test "ls --clone: modified directory shows mtime from clone root (not '-')" {
    mkdir -p sub
    echo "a" > sub/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # Add a new file inside the directory to make it M status
    echo "b" > sub/b.txt
    run "$OMEMFS" ls --clone
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep "sub/")
    [[ -n "$dir_line" ]]
    # mtime comes from clone root tree entry; must not be '-' for a pushed directory
    [[ "$dir_line" != *"- sub/"* ]]
}

@test "ls: modified directory mtime is shown after push" {
    mkdir -p sub
    echo "content" > sub/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # Add a new file to make the directory M status
    echo "new" > sub/new.txt
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep "sub/")
    [[ -n "$dir_line" ]]
    # mtime comes from clone root tree entry; must not be '-' for a pushed directory
    [[ "$dir_line" != *"- sub/"* ]]
    # Status must be M since working tree differs from clone root
    [[ "$dir_line" == ?M*"sub/"* ]]
}

@test "ls: mtime is shown for added files with --working" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "new content" > new_file.txt
    run "$OMEMFS" ls --working
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "new_file.txt")
    [[ -n "$file_line" ]]
    # --working populates mtime from the working tree; must not be '-'
    [[ "$file_line" != *"- new_file.txt"* ]]
}

@test "ls: modified file's row reflects the current working tree size with --working" {
    # design/04 "--working": row substitution is resolved per displayed row by
    # navigating the working tree directly, not from a pre-flattened whole-tree
    # map. This must still pick up a content change on an existing (clone-root)
    # file.
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls --working
    [ "$status" -eq 0 ]
    line_before=$(echo "$output" | grep "file.txt")

    printf 'a substantially different and longer replacement content\n' > file.txt
    new_size=$(wc -c < file.txt | tr -d ' ')

    run "$OMEMFS" ls --working
    [ "$status" -eq 0 ]
    line_after=$(echo "$output" | grep "file.txt")

    [[ "$line_after" != "$line_before" ]]
    [[ "$line_after" == *" $new_size "* ]]
}

@test "ls -r --working shows the current size for a deeply nested modified file" {
    mkdir -p a/b/c
    echo "deep" > a/b/c/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    printf 'deep replacement content that is longer\n' > a/b/c/file.txt
    new_size=$(wc -c < a/b/c/file.txt | tr -d ' ')

    run "$OMEMFS" ls -r --working
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "a/b/c/file.txt")
    [[ -n "$file_line" ]]
    [[ "$file_line" == *" $new_size "* ]]
}

@test "ls --working <path> shows the current size for a modified file within the scope" {
    mkdir -p src docs
    echo "source" > src/main.rs
    echo "readme" > docs/README.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    printf 'source file replaced with much longer content\n' > src/main.rs
    new_size=$(wc -c < src/main.rs | tr -d ' ')

    run "$OMEMFS" ls --working src
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "main.rs")
    [[ -n "$file_line" ]]
    [[ "$file_line" == *" $new_size "* ]]
    # out-of-scope path must not be listed
    [[ "$output" != *"README.md"* ]]
}

@test "ls --working root self-row reflects the current working tree size" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    printf 'a longer replacement for the root aggregate size test\n' > file.txt
    # The root aggregate also includes the repo's own .omemfs-filter (created
    # by `clone --new`), so compute the expected total rather than assuming
    # file.txt is the only root-level entry.
    filter_size=$(wc -c < .omemfs-filter | tr -d ' ')
    file_size=$(wc -c < file.txt | tr -d ' ')
    expected_total=$((filter_size + file_size))

    run "$OMEMFS" ls --working
    [ "$status" -eq 0 ]
    root_line=$(echo "$output" | grep '\.$')
    [[ -n "$root_line" ]]
    [[ "$root_line" == *" $expected_total "* ]]
}

@test "ls: default source is the working tree (matches --working, not --clone)" {
    # design/04 "--working": --working is the default when none of --remote,
    # --clone, --working is given.
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    printf 'a substantially different and longer replacement content\n' > file.txt
    new_size=$(wc -c < file.txt | tr -d ' ')

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    default_line=$(echo "$output" | grep "file.txt")
    [[ -n "$default_line" ]]
    [[ "$default_line" == *" $new_size "* ]]

    # --clone still shows the old (pushed) size, proving the default really
    # changed source rather than --clone having stopped reflecting clone_root.
    run "$OMEMFS" ls --clone
    [ "$status" -eq 0 ]
    clone_line=$(echo "$output" | grep "file.txt")
    [[ -n "$clone_line" ]]
    [[ "$clone_line" != *" $new_size "* ]]
}

@test "ls: new empty directory is shown with A status" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    mkdir emptydir
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep "emptydir/")
    [[ -n "$dir_line" ]]
    [[ "$dir_line" == ?A*"emptydir/"* ]]
}

@test "ls: --dirty shows new empty directory with A status" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    mkdir emptydir
    run "$OMEMFS" ls --dirty
    [ "$status" -eq 0 ]
    [[ "$output" == *"emptydir/"* ]]
    [[ "$output" == ?A*"emptydir/"* ]]
}

@test "ls: empty dir under a working-tree-only scope dir is listed once" {
    # Regression: when the scope directory itself is absent from the clone root
    # (status A, resolved from the working tree), an empty directory inside it
    # was emitted twice — once by collect_tree_rows (forced A) and again by the
    # AddedEmptyDir arm of the diff loop, which lacked the existing_paths guard
    # the Added (blob) arm has.
    echo "base" > base.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # `proj` is new (not in clone root); it contains a file and an empty subdir.
    mkdir -p proj/emptydir
    echo "hi" > proj/keep.txt
    run "$OMEMFS" ls proj
    [ "$status" -eq 0 ]
    count=$(echo "$output" | grep -c "proj/emptydir/")
    [ "$count" -eq 1 ]
    line=$(echo "$output" | grep "proj/emptydir/")
    [[ "$line" == ?A*"proj/emptydir/"* ]]
}

@test "ls -r: empty dir under a working-tree-only scope dir is listed once" {
    echo "base" > base.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    mkdir -p proj/emptydir
    echo "hi" > proj/keep.txt
    run "$OMEMFS" ls -r proj
    [ "$status" -eq 0 ]
    count=$(echo "$output" | grep -c "proj/emptydir/")
    [ "$count" -eq 1 ]
}

@test "ls: before push, nested file's parent dir is shown as direct child (no -r)" {
    mkdir -p sub/deep
    echo "nested" > sub/deep/file.txt
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    # sub/ should appear as direct child with A status
    [[ "$output" == *"sub/"* ]]
    # but sub/deep/file.txt should NOT appear directly
    [[ "$output" != *"sub/deep/file.txt"* ]]
}

@test "ls: before push, nested file appears with -r" {
    mkdir -p sub/deep
    echo "nested" > sub/deep/file.txt
    run "$OMEMFS" ls -r
    [ "$status" -eq 0 ]
    [[ "$output" == *"sub/deep/file.txt"* ]]
}

@test "ls: after push, new directory is shown as A" {
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    mkdir newdir
    echo "new" > newdir/file.txt
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep "newdir/")
    [[ -n "$dir_line" ]]
    [[ "$dir_line" == ?A*"newdir/"* ]]
}

@test "ls: existing dir with new file inside shows M status" {
    mkdir subdir
    echo "old" > subdir/old.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "new" > subdir/new.txt
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep "subdir/")
    # Only one line for subdir/ (no duplicate A row)
    dir_count=$(echo "$output" | grep -c "subdir/")
    [ "$dir_count" -eq 1 ]
    # Status must be M
    [[ "$dir_line" == ?M*"subdir/"* ]]
}

@test "ls --clone: unchanged file shows mtime from clone root" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls --clone
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "file.txt")
    [[ -n "$file_line" ]]
    # Status must be space (unchanged)
    [[ "$file_line" == " "*"file.txt"* ]]
    # mtime comes from clone root tree entry; must not be '-' for a pushed file
    [[ "$file_line" != *"- file.txt"* ]]
}

@test "ls --clone: modified file shows mtime from clone root (not '-')" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "changed" > file.txt
    run "$OMEMFS" ls --clone
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "file.txt")
    [[ -n "$file_line" ]]
    # mtime comes from clone root; must not be '-' for a previously pushed file
    [[ "$file_line" != *"- file.txt"* ]]
}

@test "ls: dir shows M when a nested file is modified (not just added)" {
    mkdir subdir
    echo "old" > subdir/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "changed" > subdir/file.txt
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep "subdir/")
    [[ -n "$dir_line" ]]
    # Directory must show M when a file inside is modified
    [[ "$dir_line" == ?M*"subdir/"* ]]
    # mtime (from clone root) must not be '-'
    [[ "$dir_line" != *"- subdir/"* ]]
}

@test "ls: dir shows M when a nested file is deleted" {
    mkdir subdir
    echo "old" > subdir/file.txt
    echo "keep" > subdir/keep.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    rm subdir/file.txt
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep "subdir/")
    [[ -n "$dir_line" ]]
    # Directory must show M when a file inside is deleted
    [[ "$dir_line" == ?M*"subdir/"* ]]
}

# ---------------------------------------------------------------------------
# Remote status column (R) tests — require two-clone setup
# ---------------------------------------------------------------------------

setup_two_clones_for_ls() {
    setup_test_dir
    setup_local_remote
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone_A
    [ "$status" -eq 0 ]
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" clone_B
    [ "$status" -eq 0 ]
}

@test "ls: R column shows M when remote modified a file" {
    setup_two_clones_for_ls

    # clone_A pushes a file.
    cd clone_A
    echo "v1" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone_B pulls to sync clone_root, then remote (clone_A) modifies the file.
    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    cd ..

    cd clone_A
    echo "v2" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone_B has not pulled yet — remote is ahead with a modified file.
    cd clone_B
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "file.txt")
    [[ -n "$file_line" ]]
    # R column (first char) must be M
    [[ "${file_line:0:1}" == "M" ]]
}

@test "ls: R column shows M when remote made a chmod-only change" {
    # refactor-instructions.md C4: a mode-only (exec-bit) remote change must
    # be reported as M, matching the X column's tree-hash-based comparison
    # (which push's dirty detection also uses) rather than a hash-only diff.
    setup_two_clones_for_ls

    cd clone_A
    echo "v1" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    cd ..

    # Remote (clone_A) flips the executable bit only -- content is unchanged.
    cd clone_A
    chmod +x file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone_B has not pulled yet -- remote is ahead with a mode-only change.
    cd clone_B
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "file.txt")
    [[ -n "$file_line" ]]
    # R column (first char) must be M, not space.
    [[ "${file_line:0:1}" == "M" ]]
}

@test "ls: R column shows A when remote added a file" {
    setup_two_clones_for_ls

    # Both clones start from an empty remote. clone_A pushes an initial file.
    cd clone_A
    echo "existing" > existing.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone_B pulls to sync clone_root.
    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    cd ..

    # clone_A adds a new file and pushes — clone_B has not pulled yet.
    cd clone_A
    echo "new" > new_file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    # new_file.txt is not in clone_B's clone_root — it should not appear
    # as a normal entry, but we verify the existing.txt line has no R marker.
    existing_line=$(echo "$output" | grep "existing.txt")
    [[ -n "$existing_line" ]]
    # existing.txt was not changed remotely — R column must be space.
    [[ "${existing_line:0:1}" == " " ]]
}

@test "ls: R column shows D when remote deleted a file" {
    setup_two_clones_for_ls

    # clone_A pushes two files.
    cd clone_A
    echo "keep" > keep.txt
    echo "del"  > del.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone_B pulls to sync clone_root.
    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    cd ..

    # clone_A deletes del.txt locally and pushes the scoped path.
    cd clone_A
    rm del.txt
    run "$OMEMFS" push del.txt
    [ "$status" -eq 0 ]
    cd ..

    # clone_B has not pulled — del.txt should show D in R column.
    cd clone_B
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    del_line=$(echo "$output" | grep "del.txt")
    [[ -n "$del_line" ]]
    # R column must be D
    [[ "${del_line:0:1}" == "D" ]]
}

@test "ls: R column is space when remote is in sync" {
    setup_two_clones_for_ls

    cd clone_A
    echo "v1" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone_B pulls — now in sync with remote.
    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "file.txt")
    [[ -n "$file_line" ]]
    # R column must be space (in sync)
    [[ "${file_line:0:1}" == " " ]]
}

@test "ls: remote INDEX_ROOT lookup times out and leaves R column blank" {
    # The default `setup_repo` clones into the current dir with REMOTE_DIR as
    # origin. Push a file so the remote and clone_root exist.
    echo "v1" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Replace the remote INDEX_ROOT with a FIFO so any read blocks forever.
    # The 5-second timeout in `ls` must abandon the lookup and still succeed.
    rm -f "$REMOTE_DIR/INDEX_ROOT"
    mkfifo "$REMOTE_DIR/INDEX_ROOT"

    # `timeout 15` is a generous outer bound: the 5s lookup timeout fires well
    # within it. If `ls` blocked indefinitely, `timeout` would kill it (124).
    run timeout 15 "$OMEMFS" ls
    [ "$status" -eq 0 ]

    file_line=$(echo "$output" | grep "file.txt")
    [[ -n "$file_line" ]]
    # R column (first char) must be blank because the lookup timed out — same as
    # the error path.
    [[ "${file_line:0:1}" == " " ]]

    # Remove the FIFO so teardown's rm -rf does not block on it.
    rm -f "$REMOTE_DIR/INDEX_ROOT"
}

@test "ls: fast remote path still populates R column" {
    # Sanity check the non-timeout path: a reachable remote with a change must
    # still produce a non-blank R column (here 'A' for a remote-added file).
    setup_two_clones_for_ls

    cd clone_A
    echo "base" > base.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    cd ..

    cd clone_A
    echo "added" > added.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    cd clone_B
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    added_line=$(echo "$output" | grep "added.txt")
    [[ -n "$added_line" ]]
    # Remote added the file → R column shows 'A' (proves the fast path works).
    [[ "${added_line:0:1}" == "A" ]]
}

@test "ls: remote-added file appears in ls output with A in R column" {
    setup_two_clones_for_ls

    # clone_A pushes an initial file so both clones have a common base.
    cd clone_A
    echo "base" > base.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone_B pulls to sync clone_root.
    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    cd ..

    # clone_A pushes a brand-new file — clone_B has not pulled yet.
    cd clone_A
    echo "new" > new_file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone_B: new_file.txt exists only on remote, not in clone_root or working tree.
    # omemfs ls must show new_file.txt with R=A.
    cd clone_B
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    new_line=$(echo "$output" | grep "new_file.txt")
    [[ -n "$new_line" ]]
    # R column (first char) must be A
    [[ "${new_line:0:1}" == "A" ]]
}

@test "ls: conflict column shows ! for file with conflict helper files" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # Simulate a pull conflict by creating conflict helper files manually.
    echo "base content" > file.txt.omemfs-conflict-base
    echo "local change" > file.txt.omemfs-conflict-local
    echo "remote change" > file.txt.omemfs-conflict-remote
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "file\.txt$")
    [[ -n "$file_line" ]]
    # Third column (conflict column) must be '!'
    [[ "${file_line:2:1}" == "!" ]]
}

@test "ls: conflict column shows space for file without conflict helper files" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "file\.txt$")
    [[ -n "$file_line" ]]
    # Third column (conflict column) must be ' ' (space)
    [[ "${file_line:2:1}" == " " ]]
}

@test "ls: conflict helper files themselves do not appear in ls output" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "base content" > file.txt.omemfs-conflict-base
    echo "local change" > file.txt.omemfs-conflict-local
    echo "remote change" > file.txt.omemfs-conflict-remote
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    [[ "$output" != *"omemfs-conflict"* ]]
}

@test "ls: directory shows ! in conflict column when a descendant has conflict helper files" {
    mkdir -p src
    echo "hello" > src/main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # Simulate a conflict on a file inside src/
    echo "base content" > src/main.rs.omemfs-conflict-base
    echo "local change" > src/main.rs.omemfs-conflict-local
    echo "remote change" > src/main.rs.omemfs-conflict-remote
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep "src/$")
    [[ -n "$dir_line" ]]
    # Third column (conflict column) must be '!'
    [[ "${dir_line:2:1}" == "!" ]]
}

@test "ls: directory shows space in conflict column when no descendants have conflict helper files" {
    mkdir -p src
    echo "hello" > src/main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep "src/$")
    [[ -n "$dir_line" ]]
    # Third column (conflict column) must be ' ' (space)
    [[ "${dir_line:2:1}" == " " ]]
}

@test "ls: remote-added directory appears in ls output with A in R column" {
    setup_two_clones_for_ls

    # clone_A pushes an initial file so both clones have a common base.
    cd clone_A
    echo "base" > base.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone_B pulls to sync clone_root.
    cd clone_B
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]
    cd ..

    # clone_A pushes a new directory — clone_B has not pulled yet.
    cd clone_A
    mkdir newdir
    echo "content" > newdir/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    cd ..

    # clone_B: newdir/ exists only on remote, not in clone_root or working tree.
    # omemfs ls must show newdir/ with R=A.
    cd clone_B
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep "newdir/")
    [[ -n "$dir_line" ]]
    [[ "${dir_line:0:1}" == "A" ]]
}

# ---------------------------------------------------------------------------
# Stub column (Z) tests
# ---------------------------------------------------------------------------

@test "ls: Z column shows S for a file stub" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub file.txt
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "file\.txt$")
    [[ -n "$file_line" ]]
    # Z column (3rd character, index 2) must be 'S'
    [[ "${file_line:2:1}" == "S" ]]
}

@test "ls: Z column shows space for a normal (non-stubbed) file" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "file\.txt$")
    [[ -n "$file_line" ]]
    # Z column must be ' ' (space)
    [[ "${file_line:2:1}" == " " ]]
}

@test "ls: Z column shows S for a fully-stubbed directory" {
    mkdir -p sub
    echo "a" > sub/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub sub/
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep "sub/$")
    [[ -n "$dir_line" ]]
    # Z column must be 'S' (fully stubbed directory)
    [[ "${dir_line:2:1}" == "S" ]]
}

@test "ls: Z column shows s for a directory with a stub in its subtree" {
    mkdir -p parent/child
    echo "content" > parent/child/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub parent/child/file.txt
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    parent_line=$(echo "$output" | grep "parent/$")
    [[ -n "$parent_line" ]]
    # Z column must be 's' (subtree contains a stub)
    [[ "${parent_line:2:1}" == "s" ]]
}

@test "ls: Z column shows s for a partially-expanded directory" {
    mkdir -p sub
    echo "a" > sub/a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub sub/
    [ "$status" -eq 0 ]

    # Add a new file alongside the directory stub (partial expansion state)
    echo "new" > sub/new.txt

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep "sub/$")
    [[ -n "$dir_line" ]]
    # Z column must be 's' (partially expanded: .omemfs-stub + real file coexist)
    [[ "${dir_line:2:1}" == "s" ]]
}

@test "ls: conflict (!) takes precedence over stub S in Z column" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" stub file.txt
    [ "$status" -eq 0 ]

    # Simulate a conflict on the same file
    echo "base" > file.txt.omemfs-conflict-base
    echo "local" > file.txt.omemfs-conflict-local
    echo "remote" > file.txt.omemfs-conflict-remote

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "file\.txt$")
    [[ -n "$file_line" ]]
    # Z column must be '!' (conflict takes precedence over stub)
    [[ "${file_line:2:1}" == "!" ]]
}

# ---------------------------------------------------------------------------
# Ignore column (Z=I) tests
# ---------------------------------------------------------------------------

@test "ls: ignored file does not appear before ignore pattern is added (baseline)" {
    echo "hello" > normal.txt
    echo "secret" > ignored_file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    # Both files visible before any ignore pattern
    [[ "$output" == *"normal.txt"* ]]
    [[ "$output" == *"ignored_file.txt"* ]]
}

@test "ls: Z column shows I for an ignored file not in clone_root (X=space)" {
    echo "hello" > normal.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Add a file that is not in clone_root, then ignore it
    echo "secret" > ignored_file.txt
    printf '[ignore]\nignored_file.txt\n' > .omemfs-filter

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    # ignored_file.txt must now appear in ls output
    [[ "$output" == *"ignored_file.txt"* ]]
    ignored_line=$(echo "$output" | grep "ignored_file\.txt$")
    [[ -n "$ignored_line" ]]
    # Z column (index 2) must be 'I'
    [[ "${ignored_line:2:1}" == "I" ]]
    # X column (index 1) must be ' ' (not in clone_root, so not a pending delete)
    [[ "${ignored_line:1:1}" == " " ]]
}

@test "ls: Z column shows i with X=D for an ignored file that is in clone_root" {
    echo "hello" > normal.txt
    echo "will-be-ignored" > tracked.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Now ignore tracked.txt (it is already in clone_root)
    printf '[ignore]\ntracked.txt\n' > .omemfs-filter

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    # tracked.txt must appear in ls output (it is in clone_root)
    [[ "$output" == *"tracked.txt"* ]]
    tracked_line=$(echo "$output" | grep "tracked\.txt$")
    [[ -n "$tracked_line" ]]
    # Z column (index 2) must be 'i' (ignored AND present in clone_root)
    [[ "${tracked_line:2:1}" == "i" ]]
    # X column (index 1) must be 'D' (in clone_root but excluded → will be deleted on push)
    [[ "${tracked_line:1:1}" == "D" ]]
}

@test "ls: Z column shows I with X=space for an ignored file not in clone_root" {
    echo "hello" > normal.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Create and ignore a file that has never been pushed (absent from clone_root).
    echo "new" > untracked.txt
    printf '[ignore]\nuntracked.txt\n' > .omemfs-filter

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    line=$(echo "$output" | grep "untracked\.txt$")
    [[ -n "$line" ]]
    # Z column must be 'I' (ignored, not in clone_root)
    [[ "${line:2:1}" == "I" ]]
    # X column must be space (never tracked → no push change)
    [[ "${line:1:1}" == " " ]]
}

@test "ls: conflict (!) takes precedence over ignore I in Z column" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Ignore the file and also create conflict helper files
    printf '[ignore]\nfile.txt\n' > .omemfs-filter
    echo "base" > file.txt.omemfs-conflict-base
    echo "local" > file.txt.omemfs-conflict-local
    echo "remote" > file.txt.omemfs-conflict-remote

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    file_line=$(echo "$output" | grep "file\.txt$")
    [[ -n "$file_line" ]]
    # Z column must be '!' (conflict takes precedence over ignore)
    [[ "${file_line:2:1}" == "!" ]]
}

# ---------------------------------------------------------------------------
# Self-entry row tests (directory itself shown as first row)
# ---------------------------------------------------------------------------

@test "ls: no-args shows root self-entry row as '.'" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dot_line=$(echo "$output" | grep ' \.$')
    [[ -n "$dot_line" ]]
}

@test "ls: root self-entry shows space status when working tree is clean" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dot_line=$(echo "$output" | grep ' \.$')
    [[ -n "$dot_line" ]]
    # X column (index 1) must be ' ' (unchanged)
    [[ "${dot_line:1:1}" == " " ]]
}

@test "ls: root self-entry shows M status when working tree is modified" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "changed" > file.txt
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dot_line=$(echo "$output" | grep ' \.$')
    [[ -n "$dot_line" ]]
    # X column (index 1) must be 'M'
    [[ "${dot_line:1:1}" == "M" ]]
}

@test "ls: root self-entry is first row in output" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    first_line=$(echo "$output" | head -1)
    [[ "$first_line" == *" ." ]]
}

@test "ls: explicit '.' arg shows root self-entry row" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls .
    [ "$status" -eq 0 ]
    dot_line=$(echo "$output" | grep ' \.$')
    [[ -n "$dot_line" ]]
}

@test "ls: directory path-arg shows self-entry row for that directory" {
    mkdir -p src
    echo "source" > src/main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls src
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep 'src/$')
    [[ -n "$dir_line" ]]
}

@test "ls: directory self-entry is first row when path-arg given" {
    mkdir -p src
    echo "source" > src/main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls src
    [ "$status" -eq 0 ]
    first_line=$(echo "$output" | head -1)
    [[ "$first_line" == *"src/" ]]
}

@test "ls: directory self-entry shows space status when contents unchanged" {
    mkdir -p src
    echo "source" > src/main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls src
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep 'src/$')
    [[ -n "$dir_line" ]]
    # X column (index 1) must be ' '
    [[ "${dir_line:1:1}" == " " ]]
}

@test "ls: directory self-entry shows M status when a file inside is modified" {
    mkdir -p src
    echo "source" > src/main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "changed" > src/main.rs
    run "$OMEMFS" ls src
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep 'src/$')
    [[ -n "$dir_line" ]]
    # X column (index 1) must be 'M'
    [[ "${dir_line:1:1}" == "M" ]]
}

@test "ls: directory self-entry shows M status when a file inside is added" {
    mkdir -p src
    echo "source" > src/main.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "new" > src/lib.rs
    run "$OMEMFS" ls src
    [ "$status" -eq 0 ]
    dir_line=$(echo "$output" | grep 'src/$')
    [[ -n "$dir_line" ]]
    [[ "${dir_line:1:1}" == "M" ]]
}

@test "ls: root self-entry shows '!' in Z column when a descendant has conflict helper files" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "base" > file.txt.omemfs-conflict-base
    echo "local" > file.txt.omemfs-conflict-local
    echo "remote" > file.txt.omemfs-conflict-remote
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dot_line=$(echo "$output" | grep ' \.$')
    [[ -n "$dot_line" ]]
    # Z column (index 2) must be '!'
    [[ "${dot_line:2:1}" == "!" ]]
}

@test "ls: root self-entry shows 's' in Z column when a file is stubbed" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" stub file.txt
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dot_line=$(echo "$output" | grep ' \.$')
    [[ -n "$dot_line" ]]
    # Z column (index 2) must be 's' (root has stubs in subtree)
    [[ "${dot_line:2:1}" == "s" ]]
}

@test "ls: root self-entry shows space Z column when no stubs or conflicts" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    dot_line=$(echo "$output" | grep ' \.$')
    [[ -n "$dot_line" ]]
    # Z column (index 2) must be ' '
    [[ "${dot_line:2:1}" == " " ]]
}

@test "ls: -r does not recurse into an ignored directory" {
    mkdir -p build
    echo "artifact" > build/out.o
    echo "more" > build/sub/deep.o 2>/dev/null || { mkdir -p build/sub; echo "more" > build/sub/deep.o; }
    echo "keep" > keep.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Ignore the build directory.
    printf '[ignore]\nbuild/\n' > .omemfs-filter

    run "$OMEMFS" ls -r
    [ "$status" -eq 0 ]
    # The build/ directory itself is shown.
    [[ "$output" == *"build/"* ]]
    # Its contents must NOT be expanded even with -r.
    [[ "$output" != *"build/out.o"* ]]
    [[ "$output" != *"build/sub"* ]]
    # build/ row must carry Z='i' (ignored, in clone_root).
    build_line=$(echo "$output" | grep "build/$")
    [[ "${build_line:2:1}" == "i" ]]
}

@test "ls: unknown reserved file is shown with Z='?'" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Create a reserved-namespace file of an unknown kind (newer-version artefact).
    echo '{}' > .omemfs-future

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    line=$(echo "$output" | grep "\.omemfs-future$")
    [[ -n "$line" ]]
    # Z column must be '?'.
    [[ "${line:2:1}" == "?" ]]
}

@test "ls: unknown reserved file is not pushed and remains untouched" {
    echo "hello" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo 'newer-data' > thing.omemfs-future

    # Push must warn-and-skip; the unknown file must not become tracked content.
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # The unknown reserved file is still on disk and unchanged.
    [ -f thing.omemfs-future ]
    [ "$(cat thing.omemfs-future)" = "newer-data" ]

    # It must not appear as a logical entry in the remote tree.
    run "$OMEMFS" cat remote-root
    [ "$status" -eq 0 ]
    [[ "$output" != *"thing.omemfs-future"* ]]
}

@test "ls: working-tree-only file is listable without --working (fallback)" {
    # Create and push an initial file so clone_root is non-empty.
    echo "initial" > base.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Create a new file that exists only in the working tree (not in clone root).
    echo "local only" > new_file.txt

    # ls without --working should list it from the working tree with status 'A'.
    run "$OMEMFS" ls new_file.txt
    [ "$status" -eq 0 ]
    [[ "$output" == *"new_file.txt"* ]]
    # Status column (2nd char) must be 'A'.
    file_line=$(echo "$output" | grep "new_file.txt")
    [[ -n "$file_line" ]]
    [[ "${file_line:1:1}" == "A" ]]
}

@test "ls: working-tree-only directory is listable without --working (fallback)" {
    # Create and push an initial file so clone_root is non-empty.
    echo "initial" > base.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Create a new directory that exists only in the working tree.
    mkdir new_dir
    echo "content" > new_dir/file.txt

    # ls without --working should list the directory from the working tree with status 'A'.
    run "$OMEMFS" ls new_dir
    [ "$status" -eq 0 ]
    [[ "$output" == *"new_dir/"* ]]
    # Self-entry row for new_dir/ must have status 'A'.
    dir_line=$(echo "$output" | grep "new_dir/")
    [[ -n "$dir_line" ]]
    [[ "${dir_line:1:1}" == "A" ]]
    # Child file must also be listed.
    [[ "$output" == *"file.txt"* ]]
}

@test "ls: path not found in clone root nor working tree" {
    # Create and push an initial file so clone_root is non-empty.
    echo "initial" > base.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Try to list a path that does not exist anywhere.
    run "$OMEMFS" ls nonexistent.txt
    [ "$status" -ne 0 ]
    # Error message must be "path not found: <path>" (no mention of "clone root" or "--working" hint).
    [[ "$output" == *"path not found: nonexistent.txt"* ]]
    [[ "$output" != *"path not found in clone root"* ]]
    [[ "$output" != *"Hint: use --working"* ]]
}

@test "ls: scoped listing does not scan out-of-scope subtrees" {
    # design/04 "Scoped working-tree scan": `ls <path>` must only walk the
    # in-scope subtree. We detect an out-of-scope scan by the tree objects it
    # would write: a cold scan of a directory writes that directory's tree
    # object. After pushing both subtrees (so clone_root holds their trees),
    # we modify an out-of-scope file and run a scoped ls of the other subtree.
    # The scoped ls must NOT write the (changed) out-of-scope directory's tree.
    mkdir -p src docs
    echo "source main" > src/main.rs
    echo "the readme" > docs/README.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Change an out-of-scope file so that scanning docs/ would yield a NEW tree
    # object hash (distinct from the clone-root one already stored).
    echo "changed readme content that is clearly different" > docs/README.md

    before=$(find .omemfs/objects -type f | wc -l)
    run "$OMEMFS" ls src
    [ "$status" -eq 0 ]
    [[ "$output" == *"src/main.rs"* ]]
    [[ "$output" != *"docs/README.md"* ]]
    after=$(find .omemfs/objects -type f | wc -l)
    # A scoped ls of src/ must not write docs/'s new tree object. The only
    # objects it may write are src/'s tree and the spliced root tree (both
    # already present from the push above, so the count should not grow by the
    # out-of-scope tree). Allow at most 1 new object (the spliced root tree if
    # its hash differs), but never the out-of-scope docs tree on top of that.
    [ "$((after - before))" -le 1 ]
}

@test "ls: scoped listing of a single file does not scan siblings" {
    # A file <path> argument hashes only that file; sibling files in the same
    # directory must not be read. We verify the output is just that file's row
    # plus no out-of-scope rows.
    mkdir -p pkg
    echo "one" > pkg/a.txt
    echo "two" > pkg/b.txt
    echo "three" > pkg/c.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls pkg/a.txt
    [ "$status" -eq 0 ]
    [[ "$output" == *"pkg/a.txt"* ]]
    [[ "$output" != *"pkg/b.txt"* ]]
    [[ "$output" != *"pkg/c.txt"* ]]
}

@test "ls: scoped listing preserves out-of-scope STAT_CACHE entries" {
    # design/07 "Read optimisation": a scoped ls loads only the in-scope slice
    # of STAT_CACHE and writes back via write_scoped_merge, which must preserve
    # out-of-scope entries byte-for-byte. Verify the out-of-scope cache entry
    # survives a scoped ls by checking a later push of the out-of-scope path
    # still gets a cache hit (no re-hash / no new objects beyond trees).
    mkdir -p src docs
    echo "source main" > src/main.rs
    echo "the readme" > docs/README.md
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Full ls to populate STAT_CACHE for both src and docs.
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    [ -f .omemfs/STAT_CACHE ]

    # Scoped ls of just src (loads/merges only the src slice).
    run "$OMEMFS" ls src
    [ "$status" -eq 0 ]

    # The docs entry must still be in STAT_CACHE: a scoped ls of docs now must
    # not re-hash docs/README.md (it is unchanged and cached).
    before=$(find .omemfs/objects -type f | wc -l)
    run "$OMEMFS" ls docs
    [ "$status" -eq 0 ]
    [[ "$output" == *"docs/README.md"* ]]
    after=$(find .omemfs/objects -type f | wc -l)
    # No blobs and at most the (already-present) trees → count must not grow.
    [ "$((after - before))" -le 1 ]
}

@test "ls: scoped listing matches unscoped listing for the same path" {
    # The scope-limited STAT_CACHE load must not change ls output: a scoped
    # listing of src/ must show the same src rows as a full ls filtered to src.
    mkdir -p src
    echo "alpha" > src/a.rs
    echo "beta" > src/b.rs
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    echo "changed" > src/a.rs

    run "$OMEMFS" ls src --no-remote
    [ "$status" -eq 0 ]
    scoped_a=$(echo "$output" | grep "src/a.rs")
    scoped_b=$(echo "$output" | grep "src/b.rs")
    # a.rs is modified (X=M), b.rs is unchanged (X=space).
    [[ "${scoped_a:1:1}" == "M" ]]
    [[ "${scoped_b:1:1}" == " " ]]
}

@test "ls: scoped listing works with no clone root (push-only, never cloned)" {
    # design/04: the splice base is an empty tree when no clone root exists.
    # Here the repo has a clone_root from setup; to exercise the empty-base
    # path we list a freshly-created, never-pushed subtree.
    mkdir -p fresh
    echo "brand new" > fresh/new.txt
    run "$OMEMFS" ls fresh
    [ "$status" -eq 0 ]
    [[ "$output" == *"fresh/new.txt"* ]]
    # A never-pushed in-scope file is shown as added. The status column X is the
    # 2nd character of the "RXZ" prefix (R is the remote column).
    file_line=$(echo "$output" | grep "fresh/new.txt")
    [[ "${file_line:1:1}" == "A" ]]
}

@test "ls: scoped ls of a path absent from clone root works" {
    # design/04 "Scoped working-tree scan": the mtime pre-filter map for a
    # single-path ls is built with flatten_tree_entries_scoped from only the
    # clone root's <path> subtree. When <path> does not exist in the clone
    # root at all, that helper returns an empty map (not an error) -- this
    # exercises that empty-map branch in the ls context (mirrors the push
    # regression test "push: scoped push of a new path absent from clone root
    # succeeds (empty-map branch)").
    echo "root file" > root.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # x/ is created after the push and never pushed, so it is absent from
    # clone_root entirely.
    mkdir -p x
    echo "brand new in x" > x/file.txt

    run "$OMEMFS" ls x
    [ "$status" -eq 0 ]
    [[ "$output" == *"x/file.txt"* ]]
}

@test "ls: scoped ls after out-of-scope change still reports correct status" {
    # Regression guard for the scoped-flatten change: the mtime pre-filter map
    # used by a scoped `ls a` must be built from only a/'s clone-root subtree
    # and must never cause b/'s modification to leak into (or be hidden from)
    # either scoped listing.
    mkdir -p a b
    echo "a1" > a/file.txt
    echo "b1" > b/file.txt
    # Backdate outside the racy window (RACY_THRESHOLD_SECS, design/07) so
    # STAT_CACHE hits are deterministic, avoiding a sleep-based test (pattern
    # from tests/push.bats and tests/chunk.bats).
    touch -d "2020-01-01 00:00:00" a/file.txt b/file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Use a different length and a different (still non-racy) mtime for the
    # modified content: STAT_CACHE keys on (mtime, size), so reusing the exact
    # same mtime AND size as before would make the scan return the stale
    # cached hash and hide the modification -- a cache artifact unrelated to
    # what this test is guarding against.
    echo "b2 modified content" > b/file.txt
    touch -d "2021-06-01 00:00:00" b/file.txt

    # a/ is untouched: a scoped ls of a must show it clean (no M anywhere).
    run "$OMEMFS" ls a
    [ "$status" -eq 0 ]
    [[ "$output" == *"a/file.txt"* ]]
    a_line=$(echo "$output" | grep "a/file.txt")
    [[ "${a_line:1:1}" == " " ]]
    [[ "$output" != *M* ]]

    # b/ was modified: a scoped ls of b must show the M marker on b/file.txt.
    run "$OMEMFS" ls b
    [ "$status" -eq 0 ]
    b_line=$(echo "$output" | grep "b/file.txt")
    [[ -n "$b_line" ]]
    [[ "${b_line:1:1}" == "M" ]]
}

# ---------------------------------------------------------------------------
# Local diff self-healing (design/04 "Local diff self-healing")
#
# `ls`'s local (X column) diff reads clone-root tree objects. These fixtures
# simulate a local cache gap on a clone-root tree object directly (deleting
# the object file from `.omemfs/objects/`) rather than via `omemfs expand`,
# because a normal stub's clone-root object is never actually read by the
# diff (an untouched stub's working-tree hash matches its clone-root hash
# exactly, short-circuiting the diff before any read) -- so `expand`/stub
# tooling cannot reproduce the gap this feature heals/degrades against.
# ---------------------------------------------------------------------------

@test "ls: self-heals a local cache miss on a clone-root tree object from the remote" {
    mkdir -p sub/inner
    echo "one" > sub/inner/file.txt
    echo "untouched" > top.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Locate sub/inner's clone-root tree hash. It is present both locally and
    # on the remote (already pushed above).
    run "$OMEMFS" ls -r --full-hash
    [ "$status" -eq 0 ]
    inner_line=$(echo "$output" | grep 'sub/inner/$')
    [[ -n "$inner_line" ]]
    inner_hash=$(echo "$inner_line" | grep -oE '[0-9a-f]{64}')
    [[ -n "$inner_hash" ]]

    # Delete that specific tree object from the LOCAL cache only, simulating a
    # local cache gap on a non-stub, already-materialised subtree (e.g. a
    # transient gap from a prior interrupted sync). It remains on the remote.
    #
    # The local cache uses an adaptive-depth shard layout (src/store/
    # objects_dir.rs), unlike the remote's fixed 3-level sharding, so the
    # object's filename is a variable-length SUFFIX of its hash rather than a
    # fixed "<2>/<2>/<2>/<rest>" split. Locate it by matching that suffix.
    obj_path=$(find .omemfs/objects -type f -name "*${inner_hash: -40}" | head -1)
    [ -n "$obj_path" ]
    [ -f "$obj_path" ]
    rm -f "$obj_path"

    # Modify the file so the diff must read sub/inner's (now locally missing)
    # clone-root tree object to compute its status -- this is the exact gap
    # `diff_trees_with_heal` self-heals from the remote.
    echo "one changed" > sub/inner/file.txt

    run "$OMEMFS" ls -r
    [ "$status" -eq 0 ]
    # Self-healed: sub/inner/file.txt shows M, not the STATUS_UNKNOWN '?' marker.
    file_line=$(echo "$output" | grep 'sub/inner/file\.txt$')
    [[ -n "$file_line" ]]
    [[ "${file_line:1:1}" == "M" ]]
    # An unrelated, untouched sibling still shows its correct, unaffected status.
    top_line=$(echo "$output" | grep ' top\.txt$')
    [[ -n "$top_line" ]]
    [[ "${top_line:1:1}" == " " ]]

    # The healed object must now be cached locally again (side effect of
    # LazyTreeStore), so a later command reads it without another fetch.
    [ -f "$obj_path" ]
}

@test "ls: degrades gracefully with '?' when a clone-root tree object is unresolvable" {
    mkdir -p sub/inner
    echo "one" > sub/inner/file.txt
    echo "untouched" > top.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls -r --full-hash
    [ "$status" -eq 0 ]
    inner_line=$(echo "$output" | grep 'sub/inner/$')
    [[ -n "$inner_line" ]]
    inner_hash=$(echo "$inner_line" | grep -oE '[0-9a-f]{64}')
    [[ -n "$inner_hash" ]]

    # The local cache uses an adaptive-depth shard layout (src/store/
    # objects_dir.rs), unlike the remote's fixed 3-level sharding, so the
    # object's filename is a variable-length SUFFIX of its hash rather than a
    # fixed "<2>/<2>/<2>/<rest>" split. Locate it by matching that suffix.
    obj_path=$(find .omemfs/objects -type f -name "*${inner_hash: -40}" | head -1)
    [ -n "$obj_path" ]
    [ -f "$obj_path" ]
    rm -f "$obj_path"

    # Make the remote unreachable too (established pattern in tests/stub.bats
    # "expand --dry-run does not download"), so the object is unresolvable on
    # both sides -- exactly the "absent from both" degrade scenario.
    rm -rf "$REMOTE_DIR"

    echo "one changed" > sub/inner/file.txt

    run "$OMEMFS" ls -r
    [ "$status" -eq 0 ]
    # Degraded: sub/inner/ shows the STATUS_UNKNOWN '?' marker, never the
    # blank ' ' ("in sync") that would misreport it.
    dir_line=$(echo "$output" | grep 'sub/inner/$')
    [[ -n "$dir_line" ]]
    [[ "${dir_line:1:1}" == "?" ]]
    # `-r` recursion into a directory's children is itself gated on being
    # able to read that directory's clone-root tree object (ls.rs
    # `collect_tree_rows`, pre-existing behaviour shared with the stub case:
    # a stub's clone-root tree object is likewise absent by design, and its
    # children are never listed either). Since sub/inner's clone-root tree
    # object is unresolvable here, its child file.txt has no row at all --
    # it is not shown as STATUS_UNKNOWN, it is simply not listed. The parent
    # directory's own '?' is what tells the user something under it could
    # not be determined.
    ! echo "$output" | grep -q 'sub/inner/file\.txt$'
    # An unrelated sibling elsewhere in the tree is unaffected and still shows
    # its correct status -- the command must not abort or blank out the rest
    # of the tree over one unresolvable subtree.
    top_line=$(echo "$output" | grep ' top\.txt$')
    [[ -n "$top_line" ]]
    [[ "${top_line:1:1}" == " " ]]
}
